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
mod query;
mod reduce;
mod row;
mod stage;
mod ty;
mod typed;
mod unify;

use std::sync::Arc;

// The type layer, the row-term reductions, and the `Checker` struct itself
// are shared by every sibling module. Re-exporting them here lets each module
// reach them with a single `use super::*;`, exactly as the pre-split single file
// reached every item with no qualification.
pub(crate) use reduce::*;
pub(crate) use row::*;
pub(crate) use stage::*;
pub(crate) use ty::*;

pub(crate) use facts::{child_exprs, definition_facts, DefinitionFacts};
/// The typed query: the relational skeleton a definition's stages build, with
/// each node owning the rows its operation read and produced.
///
/// Re-exported now so the module's own tests and the slices that consume it name
/// these types from one place, the way every other sibling module's items are
/// reached. Nothing outside `check` reads them yet, which is what the allowance
/// records.
#[allow(unused_imports)]
pub(crate) use query::{NodeId, QueryErr, QueryNodeKind, Row, TypedQuery};
/// The stable identity of one checked node, for matching a lowered stage to the
/// record the checker made of it.
///
/// Crate-private on purpose: it is an internal handle between the checker and
/// the lowering, and its spelling (arena positions, parser `ExprId`s) is free
/// to change. [`RowStageView::node`] is a public field of that type, so the
/// type has to be reachable from outside the crate's *code* — it is `pub` in
/// `check::stage`, and re-exported here for the crate's own modules.
pub(crate) use stage::NodeRef;
/// The query stage a definition is, and one stage application's rows.
pub use stage::{CombineKind, CombineView, RowStage, RowStageView};
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
/// A definition's body as a typed tree, and one node of it.
pub use typed::{TypedBody, TypedExpr};

pub(crate) use crate::core::ScalarType;
pub(crate) use crate::db::ModuleInput;
pub(crate) use crate::ir::{JoinKind, Phase};
pub(crate) use crate::lower::parse_module;
pub(crate) use crate::normalize::Resolved;
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
    ///
    /// Read off the definition's typed tree. A definition with no tree — a
    /// `sql` template, whose signature is its type, or one the checker never
    /// reached — leaves none open, because an open overload needs an
    /// expression to be open at.
    pub fn holes(&self, module: usize, def: usize) -> usize {
        self.typed_body(module, def).map_or(0, |b| b.holes)
    }

    /// Choice for hole `k` at use site `site` inside definition `(module, def)`.
    ///
    /// `site` is an [`ExprId`](cagara_syntax::ast::ExprId) — the `id` of the
    /// *use expression* — not a span offset. Both are small integers, so a
    /// caller that passes a byte offset gets `None` at best and, when the two
    /// happen to collide, **another use's choice**. Prefer
    /// [`TypeCheck::choices_of`], which takes the pairs the checker actually
    /// recorded and cannot be called with the wrong key.
    ///
    /// The choice is read off the node the use was written at; `def` is
    /// accepted for that key's sake but is not needed to find it, because an
    /// `ExprId` already identifies the node.
    pub fn choice(&self, module: usize, def: usize, site: u32, k: usize) -> Option<Choice> {
        let _ = def;
        self.typed_node(module, site)?
            .choices
            .iter()
            .find(|(hole, _)| *hole == k)
            .map(|(_, choice)| *choice)
    }

    /// Every resolved overload choice of a definition, as
    /// `(use site, hole, choice)` in a deterministic order.
    ///
    /// `use site` is the `ExprId` of the use expression. This is the accessor
    /// for a phase that wants *all* the choices of a definition and has no way
    /// to know the site ids in advance; it reads the same nodes
    /// [`TypeCheck::choice`] does, so the two cannot disagree.
    pub fn choices_of(&self, module: usize, def: usize) -> Vec<(u32, usize, Choice)> {
        let Some(body) = self.typed_body(module, def) else {
            return Vec::new();
        };
        // Every node's choices, in the order the tree holds them: the walk
        // assigns `ExprId`s in source order and the arena follows it, so this
        // is a reading order without a sort.
        let mut out: Vec<(u32, usize, Choice)> = body
            .nodes()
            .iter()
            .flat_map(|node| {
                node.choices
                    .iter()
                    .map(move |(hole, choice)| (node.id, *hole, *choice))
            })
            .collect();
        out.sort_by_key(|&(site, k, _)| (site, k));
        out
    }

    /// The stage nodes a definition recorded, in application order.
    ///
    /// Each node is one query-stage application: the operation it applied, the
    /// row it read, and the row it produced. Adjacent nodes chain — a stage's
    /// input row is the previous stage's output row — which is the boundary a
    /// later pass must not have to recover from a composed type.
    ///
    /// Read off the tree, at the application node that recorded each stage. The
    /// order comes from the body's stage index, because a chain is recorded in
    /// application order while the tree's arena follows source order.
    ///
    /// Empty for a definition that applies no stage, and for one that failed.
    pub fn row_stages(
        &self,
        module: usize,
        def: usize,
    ) -> impl Iterator<Item = &RowStageView> + '_ {
        self.typed_body(module, def)
            .into_iter()
            .flat_map(TypedBody::stages)
            .filter_map(|node| node.stage.as_ref())
    }

    /// The stable identity of the stage this definition recorded at `site`.
    ///
    /// `site` is an [`ExprId`](cagara_syntax::ast::ExprId) of an application node
    /// inside `(module, def)`. The answer names the node the checker recorded
    /// the application at, so a consumer holding a lowered stage can ask whether
    /// it is *the same application* rather than whether the two spans overlap.
    ///
    /// `None` when the definition records no stage at that node — a helper's
    /// use, a `table`, a join — which is a fact and not a failure.
    ///
    /// Read from the same stage list [`TypeCheck::row_stages`] yields, so the
    /// two cannot disagree about which applications exist.
    pub(crate) fn stage_node(&self, module: usize, def: usize, site: u32) -> Option<NodeRef> {
        self.row_stages(module, def)
            .find(|s| s.site == site)
            .map(|s| s.node)
    }

    /// A definition's body as a typed tree, if the checker built one.
    ///
    /// This is the checker's own account of the definition, node by node: the
    /// type each expression got, the row it is a query over, what a name
    /// resolved to, and which sub-expressions belong to which node. A consumer
    /// walks this instead of walking the AST and asking the maps what they saw.
    ///
    /// `None` for a definition the checker did not reach — a `sql` template,
    /// whose signature is its type, or one that failed before any expression
    /// was typed.
    pub fn typed_body(&self, module: usize, def: usize) -> Option<&TypedBody> {
        self.modules.get(module)?.typed.get(&(module, def))
    }

    /// The typed node for one expression of a module, found by `ExprId`.
    ///
    /// `Facts` is keyed by `ExprId` alone while a tree belongs to one
    /// definition, so this is how a reader that knows only the id reaches the
    /// node. The index holds positions, not facts.
    pub fn typed_node(&self, module: usize, id: u32) -> Option<&TypedExpr> {
        let checked = self.modules.get(module)?;
        let at = checked
            .typed_index
            .binary_search_by_key(&id, |(id, _, _)| *id)
            .ok()?;
        let (_, def, node) = checked.typed_index[at];
        checked.typed.get(&(module, def))?.nodes().get(node)
    }

    /// The typed node for one expression of a module, found by the span the
    /// checker filed it under.
    ///
    /// The span twin of [`TypeCheck::typed_node`]: that one is asked with an
    /// `ExprId`, this with the key of [`TypeCheck::use_type`]. The index holds
    /// positions, not facts.
    pub fn typed_node_at(&self, module: usize, span: Span) -> Option<&TypedExpr> {
        let checked = self.modules.get(module)?;
        let at = checked
            .typed_spans
            .binary_search_by_key(&(span.start, span.end), |(start, end, _, _)| (*start, *end))
            .ok()?;
        let (_, _, def, node) = checked.typed_spans[at];
        checked.typed.get(&(module, def))?.nodes().get(node)
    }

    /// Printed type of the name use whose expression spans `span` in `module`.
    ///
    /// Read off the node the checker filed that span under. The tree keeps the
    /// last write for a repeated key span, which is the tie-break the map this
    /// replaces used, so the answer is the same one.
    pub fn use_type(&self, module: usize, span: Span) -> Option<&str> {
        Some(self.typed_node_at(module, span)?.shown.as_str())
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
        self.typed_node(module, id)?.ty()
    }

    /// The `ExprId`s the checker recorded a type for in `module`, ascending.
    ///
    /// For a caller that wants to know what is available before walking.
    ///
    /// The typed tree covers every node of a definition, typed or not, because
    /// a walk wants the shape. This lists the subset with a recorded type: a
    /// definition that failed partway leaves its untyped nodes in the tree with
    /// no view, and an id from one of those would answer `None` to every fact
    /// reader — a node that looks available and has nothing to say.
    pub fn use_ids(&self, module: usize) -> Vec<u32> {
        let Some(checked) = self.modules.get(module) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = checked
            .typed_index
            .iter()
            .map(|(id, _, _)| *id)
            .filter(|id| self.use_scheme(module, *id).is_some())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// What the module-scope name occurrence with this `ExprId` resolved to.
    ///
    /// The identity counterpart of [`TypeCheck::use_ty`]. It answers "which
    /// definition did this name mean" where `use_ty` answers "what type did
    /// this expression get", and a consumer expanding a call needs both: the
    /// type says what the result is, the resolution says what to expand.
    ///
    /// `None` means the occurrence was not a module-scope name — a column
    /// reference, a literal, a lambda-bound parameter — or the checker never
    /// got as far as resolving it. The two are deliberately not distinguished,
    /// matching [`TypeCheck::use_ty`]: both mean "nothing was recorded here".
    pub fn resolution(&self, module: usize, id: u32) -> Option<&Resolved> {
        self.typed_node(module, id)?.resolved.as_ref()
    }

    /// Every `ExprId` in `module` the checker recorded a resolution for.
    pub fn resolved_ids(&self, module: usize) -> Vec<u32> {
        let Some(checked) = self.modules.get(module) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = checked
            .typed_index
            .iter()
            .map(|(id, _, _)| *id)
            .filter(|id| self.resolution(module, *id).is_some())
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
    /// This is what a call site needs: a definition's scheme is where its
    /// result is declared.
    pub fn result_scalar(&self, module: usize, def: usize) -> Option<ScalarType> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        scheme_result_scalar(&s.ty)
    }

    pub fn scheme_view(&self, module: usize, def: usize) -> Option<SchemeView> {
        let s = self.modules.get(module)?.schemes.get(&(module, def))?;
        Some(scheme_view(&s.ty))
    }

    /// The shape of the type the checker recorded for one **expression**.
    ///
    /// [`TypeCheck::use_ty`] answers "what scalar is this node", and by
    /// construction answers `None` for a query, a function, or a row — it
    /// filters those out. But the checker records the *whole* type for every use
    /// (`infer.rs` inserts `self.resolve(&t)` per `ExprId`, and `uses` carries
    /// the same value), so `table "s" "t"` is recorded as
    /// `query { id = int, name = string }` and `select { .name }` as
    /// `query { name = string }`. That row is exactly what a lowering of the
    /// relational primitives needs, and it is a fact about the node rather than
    /// something to be recovered by re-walking source or re-reading a
    /// definition's signature.
    ///
    /// This is the accessor for a consumer that needs *structure* rather than a
    /// scalar: a phase building a typed tree walks the AST and asks "what did
    /// this node get", and for a query-valued node the answer includes its row.
    ///
    /// `None` when the checker recorded nothing for that id — the node is not
    /// one it typed, or checking did not reach it.
    pub fn use_scheme(&self, module: usize, id: u32) -> Option<SchemeView> {
        self.typed_node(module, id)?.view.clone()
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
        probe_fields: out.probe_fields,
        typed: out.typed,
        typed_index: out.typed_index,
        typed_spans: out.typed_spans,
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
    probe_fields: Option<(usize, Vec<(String, String)>)>,
    /// Each definition's body as a typed tree, by `(module, def)`.
    typed: HashMap<(usize, usize), TypedBody>,
    /// `ExprId` to the definition that has it and the node's place in that
    /// body, ascending by id. Positions only: the facts stay on the node, and
    /// this is what lets an id-keyed reader find one without scanning every
    /// definition.
    typed_index: Vec<(u32, usize, usize)>,
    /// Key span to the definition that has it and the node's place in that
    /// body, ascending by span. The span twin of `typed_index`, and the same
    /// rule: positions only, facts on the node. A definition's body occupies
    /// one range of the module text, so two definitions cannot share a key
    /// span; a repeated span within one body was already resolved by the tree
    /// keeping its last write.
    typed_spans: Vec<(u32, u32, usize, usize)>,
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
    probe_fields: Option<(usize, Vec<(String, String)>)>,
    /// Each definition's body as a typed tree, by `(module, def)`.
    typed: HashMap<(usize, usize), TypedBody>,
    /// `ExprId` to the definition that has it and the node's place in that
    /// body, ascending by id. Positions only: the facts stay on the node, and
    /// this is what lets an id-keyed reader find one without scanning every
    /// definition.
    typed_index: Vec<(u32, usize, usize)>,
    /// Key span to the definition that has it and the node's place in that
    /// body, ascending by span. See [`ModuleOutput::typed_spans`].
    typed_spans: Vec<(u32, u32, usize, usize)>,
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
        probe: None,
        probe_fields: None,
        uses: Vec::new(),
        resolutions: HashMap::new(),
        row_stages: Vec::new(),
        combines: Vec::new(),
        combine_partials: HashMap::new(),
        holes: HashMap::new(),
        choices: HashMap::new(),
        typed_bodies: HashMap::new(),
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
    let typed = c.typed_bodies;
    let mut typed_index: Vec<(u32, usize, usize)> = Vec::new();
    for (&(_, def), body) in &typed {
        for (at, node) in body.nodes().iter().enumerate() {
            typed_index.push((node.id, def, at));
        }
    }
    typed_index.sort_unstable_by_key(|(id, _, _)| *id);
    let mut typed_spans: Vec<(u32, u32, usize, usize)> = Vec::new();
    for (&(_, def), body) in &typed {
        for &(start, end, at) in body.span_index() {
            typed_spans.push((start, end, def, at));
        }
    }
    typed_spans.sort_unstable_by_key(|(start, end, _, _)| (*start, *end));
    ModuleOutput {
        schemes: c.schemes,
        errors: c.errors,
        types,
        probe_fields,
        typed,
        typed_index,
        typed_spans,
    }
}

#[cfg(test)]
mod tests;
