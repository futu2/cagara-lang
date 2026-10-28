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
//! `ty`, `reduce`, `unify`, `overload`, `infer`, and `print`.

mod core_facts;
mod facts;
mod infer;
mod overload;
mod print;
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
pub(crate) use crate::primitive::Prim;
pub(crate) use crate::resolve::{module_own, module_scope};
pub(crate) use crate::rules::{self};
pub(crate) use crate::workspace::{Binding, Diag, Workspace};
pub(crate) use cagara_syntax::ast::{self, ExprKind, Side, Span, TypeExpr};
pub(crate) use core_facts::declared_template_kind;
pub use core_facts::{DefId, Resolved, TemplateKind};
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
    kind: RawErrorKind,
}

/// What a raw error is about, where aggregating per-definition checks has to
/// treat it differently from an error about one definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawErrorKind {
    /// Something wrong with the definition itself.
    Def,
    /// The chain of forward-referenced definitions this one lies on is too deep
    /// to check. Every definition on such a chain hits the limit in its own
    /// check, so one broken chain would otherwise be reported once per
    /// definition on it; the module keeps only the first.
    Chain,
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
        self.modules
            .get(module)?
            .typed
            .get(&(module, def))
            .map(Arc::as_ref)
    }

    /// How far the spans the checker recorded for `def` sit from the module's
    /// current text.
    ///
    /// Normally zero. It is nonzero for a definition the workspace kept through
    /// an edit that only *moved* it: `Workspace::sync_defs` compares a
    /// definition's text, so an untouched definition below an insertion keeps
    /// its memoized check — and that check, and this tree, recorded the spans of
    /// the text it was parsed from. Every recorded span moves together, so one
    /// offset puts them back; the readers that answer in the module's current
    /// coordinates (`typed_node_at`, the rendered diagnostics, the typed-span
    /// index) add it themselves, and the two places in normalization that build
    /// an origin from a *typed* span add it here.
    pub fn def_shift(&self, module: usize, def: usize) -> i64 {
        self.modules
            .get(module)
            .and_then(|checked| checked.shifts.get(def))
            .copied()
            .unwrap_or(0)
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
    check_modules(ws, None)
}

/// Type-check every definition of the modules the root can see.
///
/// What the editor needs, and nothing else. A workspace holds one module graph
/// per open document — forty tabs are forty graphs — and checking all of them
/// to publish one document's diagnostics is work proportional to the session
/// rather than to the document: cold, that was most of a save's cost on a
/// project whose library files run to megabytes. Checking reads the same
/// memoized per-module queries either way, so a module that is reached is
/// checked exactly as often as before; the modules that are not are left alone.
///
/// A module outside the root's closure is filled in with an empty result, so
/// indices still name the same module and every reader that asks about a
/// module the root cannot see gets "nothing checked" rather than a wrong
/// module's answer.
pub fn check_reachable(ws: &Workspace) -> TypeCheck {
    let reachable = ws.modules_reachable_from(ws.root);
    check_modules(ws, Some(&reachable))
}

fn check_modules(ws: &Workspace, only: Option<&[bool]>) -> TypeCheck {
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
    for (m, &input) in ws.inputs.iter().enumerate() {
        if only.is_some_and(|only| !only[m]) {
            out.modules.push(Arc::new(ModuleCheck::empty()));
            continue;
        }
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

/// The columns a column probe at [`PROBE_FIELD`] reported, if any.
///
/// The language server's completion writes [`PROBE_FIELD`] where the cursor is
/// and asks what row that position sees. The probe module is a throwaway input
/// in the caller's database, so this is the same memoized per-module check
/// every other query uses, and the live module graph is untouched.
pub fn probe_fields(db: &dyn salsa::Database, input: ModuleInput) -> Option<Vec<(String, String)>> {
    module_check(db, input)
        .probe_fields
        .as_ref()
        .map(|(_, fields)| fields.clone())
}

#[cfg(test)]
thread_local! {
    static CHECK_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many definition checks ran on this thread (for incrementality tests).
#[cfg(test)]
pub(crate) fn check_runs() -> usize {
    CHECK_RUNS.with(|c| c.get())
}

/// The modules a module may use: its prelude and its imports, in that order.
fn imported_modules(db: &dyn salsa::Database, input: ModuleInput) -> Vec<ModuleInput> {
    input
        .prelude(db)
        .iter()
        .copied()
        .chain(input.imports(db).iter().map(|(_, t)| *t))
        .collect()
}

/// Check one definition (memoized). Dependencies are checked first; only their
/// self-contained schemes are used.
///
/// The unit of incrementality. A module's definitions are checked one at a
/// time, each reading its own `DefInput` and — for a name defined above it —
/// that definition's own check. An edit inside one definition therefore
/// invalidates that check and the checks of the definitions that read it, and
/// nothing else: a keystroke in a thousand-definition file costs one
/// definition, not a thousand.
///
/// A name defined *below* its use is still checked inline, exactly as before.
/// That is what lets a forward reference close a cycle; routing it through
/// salsa would turn `q = q` into a salsa cycle instead of the diagnostic the
/// compiler reports.
#[salsa::tracked(returns(ref))]
fn def_check(db: &dyn salsa::Database, input: ModuleInput, def: usize) -> Arc<DefCheck> {
    #[cfg(test)]
    CHECK_RUNS.with(|c| c.set(c.get() + 1));
    let imported = imported_modules(db, input);
    let m = *input.index(db);
    let env = ModuleEnv {
        module: m,
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
    let mut c = Checker::new(env, db, input, def);
    c.def_scheme(m, def);
    let failed = c.failed.contains(&(m, def));
    let scheme = c.schemes.get(&(m, def)).cloned();
    let types = scheme
        .as_ref()
        .filter(|_| !failed)
        .map(|scheme| c.show_scheme(scheme));
    let typed = c.typed_bodies.remove(&(m, def)).map(Arc::new);
    // Only this definition's errors: a forward reference checked inline is
    // attributed to *its* own definition, and that definition's check reports
    // it. Passing it on here would report it twice.
    let errors: Vec<RawTypeError> = c.errors.iter().filter(|e| e.def == def).cloned().collect();
    Arc::new(DefCheck {
        scheme,
        failed,
        types,
        typed,
        errors,
        probe_fields: c.probe_fields,
    })
}

/// Check every definition of a module (memoized), one `def_check` each.
///
/// Results are collected in source order, which is the order diagnostics are
/// reported in.
#[salsa::tracked(returns(ref))]
fn module_check(db: &dyn salsa::Database, input: ModuleInput) -> Arc<ModuleCheck> {
    let m = *input.index(db);
    let count = input.defs(db).len();
    let mut schemes = HashMap::new();
    let mut types = HashMap::new();
    let mut errors = Vec::new();
    let mut typed = HashMap::new();
    let mut probe_fields = None;
    let mut chain_reported = false;
    for def in 0..count {
        let checked = def_check(db, input, def);
        if let Some(scheme) = &checked.scheme {
            schemes.insert((m, def), scheme.clone());
        }
        if let Some(t) = &checked.types {
            types.insert((m, def), t.clone());
        }
        for error in &checked.errors {
            // One chain is one problem, however many of its definitions report
            // it (see `RawErrorKind::Chain`).
            if error.kind == RawErrorKind::Chain {
                if chain_reported {
                    continue;
                }
                chain_reported = true;
            }
            errors.push(error.clone());
        }
        if let Some(body) = &checked.typed {
            typed.insert((m, def), body.clone());
        }
        if probe_fields.is_none() {
            probe_fields = checked.probe_fields.clone().map(|fields| (m, fields));
        }
    }
    // Render the diagnostics here, once per module text, rather than in every
    // `check()`. This is the one step that cannot move into `def_check`: it
    // reads the whole file's text, which would make every definition's check
    // depend on every edit.
    //
    // The text a definition's check was made against is not always the text
    // here. `Workspace::sync_defs` keeps the input of a definition whose own
    // text did not change when something above it did, so its memoized check
    // recorded spans that are stale by exactly the distance it moved — and
    // that is the whole reason a keystroke above a definition no longer
    // re-checks it. The two spans are both known here (the parse of *this*
    // text, and the span the kept input was parsed from), so the offset is
    // applied where the spans are rendered: every inner span of a definition
    // moved with it, so one delta covers the definition's diagnostics and its
    // typed tree.
    let path = input.path(db);
    let text = input.file(db).text(db);
    let starts = crate::workspace::line_starts(text);
    let current: Vec<Span> = parse_module(db, *input.file(db))
        .module
        .defs
        .iter()
        .map(|d| d.span)
        .collect();
    let moved = |def: usize| -> i64 {
        let kept = input.defs(db)[def].def(db).0.span.start;
        let now = current.get(def).map_or(kept, |span| span.start);
        i64::from(now) - i64::from(kept)
    };
    let shifts: Vec<i64> = (0..count).map(moved).collect();
    let place = |span: Span, def: usize| -> (usize, usize) {
        let delta = moved(def);
        let at = |offset: u32| (i64::from(offset) + delta).max(0) as usize;
        (at(span.start), at(span.end))
    };
    let errors = errors
        .into_iter()
        .map(|e| {
            let (start, end) = place(e.span, e.def);
            TypeError {
                module: e.module,
                def: e.def,
                diag: crate::workspace::make_diag_indexed(
                    path, text, &starts, start, end, e.message,
                ),
            }
        })
        .collect();
    let mut typed_index: Vec<(u32, usize, usize)> = Vec::new();
    for (&(_, def), body) in &typed {
        for (at, node) in body.nodes().iter().enumerate() {
            typed_index.push((node.id, def, at));
        }
    }
    typed_index.sort_unstable_by_key(|(id, _, _)| *id);
    let mut typed_spans: Vec<(u32, u32, usize, usize)> = Vec::new();
    for (&(_, def), body) in &typed {
        let delta = moved(def);
        let at = |offset: u32| (i64::from(offset) + delta).max(0) as u32;
        for &(start, end, at_node) in body.span_index() {
            typed_spans.push((at(start), at(end), def, at_node));
        }
    }
    typed_spans.sort_unstable_by_key(|(start, end, _, _)| (*start, *end));
    Arc::new(ModuleCheck {
        schemes,
        errors,
        types,
        probe_fields,
        typed,
        typed_index,
        typed_spans,
        shifts,
    })
}

/// What checking one definition produced, before its diagnostics are rendered.
///
/// Rendering needs the module's path and text, which the pure checker does not
/// have; [`module_check`] converts this into a [`ModuleCheck`].
#[derive(Debug, Clone, PartialEq)]
struct DefCheck {
    scheme: Option<Scheme>,
    /// Whether the definition's own check failed. It still gets a scheme, so a
    /// use of it does not cascade, but no type is printed for it.
    failed: bool,
    /// Its printed type, if it has one.
    types: Option<String>,
    /// Its body as a typed tree. Behind an `Arc` so that collecting a
    /// module's checks shares the trees instead of copying them: a deep copy of
    /// a thousand of them was most of the cost of a request.
    typed: Option<Arc<TypedBody>>,
    /// Type errors reported at this definition.
    errors: Vec<RawTypeError>,
    /// The row a `PROBE_FIELD` column saw here, if this is the definition that
    /// referenced it.
    probe_fields: Option<Vec<(String, String)>>,
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
    typed: HashMap<(usize, usize), Arc<TypedBody>>,
    /// `ExprId` to the definition that has it and the node's place in that
    /// body, ascending by id. Positions only: the facts stay on the node, and
    /// this is what lets an id-keyed reader find one without scanning every
    /// definition.
    typed_index: Vec<(u32, usize, usize)>,
    /// Key span to the definition that has it and the node's place in that
    /// body, ascending by span. See [`def_check`].
    typed_spans: Vec<(u32, u32, usize, usize)>,
    /// Per definition, the distance from the spans its check recorded to this
    /// text's (see [`TypeCheck::def_shift`]). Zero for a definition whose own
    /// text was edited, which is the common case.
    shifts: Vec<i64>,
}

impl ModuleCheck {
    /// A module no check has run on: what a [`TypeCheck`] holds for a module
    /// the root cannot see, and what an unchecked module answers — nothing.
    fn empty() -> Self {
        ModuleCheck {
            schemes: HashMap::new(),
            errors: Vec::new(),
            types: HashMap::new(),
            probe_fields: None,
            typed: HashMap::new(),
            typed_index: Vec::new(),
            typed_spans: Vec::new(),
            shifts: Vec::new(),
        }
    }
}

/// What checking one module reads: its definitions and scope, and every
/// module's exports (for `alias.name`). Nothing else of the workspace is
/// needed, so a salsa query can build this view.
pub(crate) struct ModuleEnv<'w> {
    pub(crate) module: usize,
    pub(crate) scope: &'w HashMap<String, Binding>,
    /// Exports of the modules it imports, by module index.
    pub(crate) owns: HashMap<usize, &'w HashMap<String, Binding>>,
    /// Borrowed checked exports; looking up one name never copies a catalog.
    pub(crate) schemes: HashMap<usize, &'w HashMap<(usize, usize), Scheme>>,
}

#[cfg(test)]
mod tests;
