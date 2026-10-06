//! The checked layer: typed relational construction.
//!
//! This is the target design's "one place for relational validity". Today a
//! stage is constrained three times — row equations in `check/`, runtime
//! checks in `prims.rs`, and column/phase checks in `schema.rs`. Here the
//! constructor *is* the rule: a [`CheckedQuery`] that exists has an output row
//! that matches its node, so no later phase has to re-derive anything.
//!
//! The layer is deliberately boring:
//!
//!   * every node carries its [`Origin`], so an error names the node that
//!     created the invalid operation without changing the tree's shape;
//!   * every constructor is `pub fn … -> Result<…, Error>`, checks its local
//!     rule against [`rules`], and returns the output row;
//!   * [`erase`] lowers a tree back to the existing [`Rel`], so the SQL
//!     backend and `schema::schema` keep working unchanged while the
//!     constructors take over the checking.
//!
//! Phase and join-side rules come from `crate::rules` — the module the
//! checker's *types* already use — so this is one statement of the rule, not a
//! third one. Column-level name rules come from `crate::schema`.

use crate::check::{Choice, SchemeView, TypeCheck};
use crate::core::{Diagnostic, Error, Origin, RowType, ScalarType};
use crate::ir::{Bound, Expr, Frame, JoinKind, Lit, Loc, Phase, Rel, SetKind, WinSpec};
use crate::rules::{self, Place};
use cagara_syntax::ast::{self, Side};
use std::collections::HashMap;

/// One checked definition: its name, its printed scheme, how many overload
/// holes it leaves open, and one core body per instantiation of those holes.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedDef {
    pub name: String,
    pub def: usize,
    /// The definition's printed type scheme (`TypeCheck::type_of`), if it was
    /// well typed. This is the string a *user* reads.
    pub scheme: Option<String>,
    /// The definition's scalar type, as closed data (`TypeCheck::def_scalar`).
    /// `None` when the scheme has no scalar reading (a function or a row).
    /// This is what a *phase* should read: unlike `scheme` it carries no
    /// inference variable.
    pub scalar: Option<ScalarType>,
    /// The definition's output row, when its scheme is a query over a closed
    /// row (`TypeCheck::scheme_fields`).
    ///
    /// `None` and `Some(vec![])` are different answers and must not be
    /// collapsed: `None` means the scheme is not a closed row at all (a
    /// function, a scalar, a still-open row), whereas `Some(vec![])` means it
    /// is a row with no columns. A caller asking "does this definition have a
    /// row?" wants the distinction.
    pub row: Option<RowType>,
    /// How many open overloads the definition leaves to its users.
    pub holes: usize,
    /// What elaboration produced for this definition, or `None` when the
    /// program was not elaborated at all.
    ///
    /// The `Option` and the [`Elaborated`] inside it are both load-bearing.
    /// `None` means "this program was not elaborated"; an unelaborated program
    /// used to carry an empty `Vec` here, so a caller could not tell "nothing
    /// elaborated" from "no bodies". `Some(Elaborated::Value)` then says the
    /// definition was elaborated and is not a query — a scalar or a function —
    /// while `Some(Elaborated::Failed)` says it could not be evaluated, with
    /// the reason in `CheckedProgram::diagnostics`. Collapsing those was the
    /// previous behaviour, and it made an evaluator error indistinguishable
    /// from a definition that legitimately has no relational term.
    ///
    /// `CheckedProgram::of` does not elaborate, so it leaves this `None`;
    /// [`CheckedProgram::of_elaborated`] fills it in.
    pub terms: Option<crate::eval::Elaborated>,
}

impl CheckedDef {
    /// The definition's core terms, when it is a query that was elaborated.
    ///
    /// `None` covers every other case — not elaborated, not a query, failed —
    /// so callers that want terms should also consult
    /// [`CheckedDef::terms`] when the *reason* matters.
    pub fn query_terms(&self) -> Option<&[crate::CoreTerm]> {
        match self.terms.as_ref()? {
            crate::eval::Elaborated::Query(ts) => Some(ts),
            _ => None,
        }
    }
}

impl CheckedDef {
    /// The definition's output row, if its scheme is a closed row.
    ///
    /// `Some(row)` includes the empty row (a query over no columns); use this
    /// rather than comparing against `vec![]` to ask "is this a row?".
    pub fn row(&self) -> Option<&RowType> {
        self.row.as_ref()
    }

    /// The *names* of the definition's output row, empty when it has none.
    /// Prefer [`CheckedDef::row`] when the distinction matters.
    pub fn row_names(&self) -> Vec<String> {
        self.row.as_ref().map(RowType::names).unwrap_or_default()
    }
}

/// One checked module: everything `TypeCheck` holds for it, in one value.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedModule {
    pub index: usize,
    pub path: String,
    pub defs: Vec<CheckedDef>,
    /// Definition index → printed scheme, for every well-typed definition.
    pub types: HashMap<usize, String>,
    /// Definition index → number of open overload holes.
    pub holes: HashMap<usize, usize>,
    /// `(def, site, hole)` → the resolved choice.
    pub choices: HashMap<(usize, u32, usize), Choice>,
    /// The columns known at this module's `PROBE_FIELD` reference, if it has
    /// one: `(name, printed type)` in row order.
    pub probe_fields: Option<Vec<(String, String)>>,
    /// `(span start, span end)` → printed type of the name use there.
    pub use_types: HashMap<(u32, u32), String>,
}

impl CheckedModule {
    pub fn new(index: usize, path: impl Into<String>) -> Self {
        CheckedModule {
            index,
            path: path.into(),
            defs: vec![],
            types: HashMap::new(),
            holes: HashMap::new(),
            choices: HashMap::new(),
            probe_fields: None,
            use_types: HashMap::new(),
        }
    }

    /// The definition with this index, if the module has one.
    pub fn def(&self, def: usize) -> Option<&CheckedDef> {
        self.defs.iter().find(|d| d.def == def)
    }

    /// The choice recorded at `site`/hole `k` of `def`.
    pub fn choice(&self, def: usize, site: u32, k: usize) -> Option<Choice> {
        self.choices.get(&(def, site, k)).copied()
    }
}

/// A whole checked program: every module, and every diagnostic.
///
/// This is the coherent single input the later phases take, replacing the
/// split where `TypeCheck` holds the choices and the `Evaluator` interprets
/// the original AST to consume them.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedProgram {
    pub modules: Vec<CheckedModule>,
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckedProgram {
    pub fn new() -> Self {
        CheckedProgram {
            modules: vec![],
            diagnostics: vec![],
        }
    }

    /// Gather what the checker learned into one checked program, without
    /// elaborating definition bodies.
    ///
    /// This runs the existing checker and groups its output per module; it does
    /// **not** change what is checked or in what order. Every
    /// [`CheckedDef::terms`] is left `None`, because elaborating a definition
    /// to a `CoreTerm` means evaluating it, and this function arranges rather
    /// than evaluates. Use [`CheckedProgram::of_elaborated`] when the bodies
    /// are needed.
    ///
    /// Everything else here *is* filled in from the checker: the printed
    /// scheme, the closed `ScalarType` and row, the open-overload count, and
    /// the resolved overload choices — so a caller that does not need bodies
    /// gets a complete picture of what was checked.
    pub fn of(ws: &crate::workspace::Workspace) -> Self {
        let tc = crate::check::check(ws);
        Self::from_type_check(ws, &tc, None)
    }

    /// [`CheckedProgram::of`], with definition bodies elaborated.
    ///
    /// Elaboration goes through the evaluator, which is why it is a separate
    /// entry point rather than something `of` does implicitly: a caller that
    /// only wants types should not pay for evaluation.
    pub fn of_elaborated(ws: &crate::workspace::Workspace) -> Self {
        let tc = crate::check::check(ws);
        let (bodies, eval_diags) = crate::eval::elaborate_bodies(ws, &tc);
        let mut out = Self::from_type_check(ws, &tc, Some(bodies));
        // The checker's diagnostics are in `out` already; these are the ones
        // evaluation raised, which the previous version dropped on the floor.
        out.diagnostics.extend(eval_diags);
        out.diagnostics.sort_by(|a, b| {
            (a.module, a.diag.line, a.diag.col, &a.diag.message).cmp(&(
                b.module,
                b.diag.line,
                b.diag.col,
                &b.diag.message,
            ))
        });
        out.diagnostics.dedup();
        out
    }

    /// [`CheckedProgram::of`], reusing a type check the caller already ran.
    ///
    /// `bodies` is `None` for "not elaborated", which is recorded as
    /// `CheckedDef::terms == None` rather than as an empty list: those are
    /// different facts and a caller needs to tell them apart.
    pub fn from_type_check(
        ws: &crate::workspace::Workspace,
        tc: &TypeCheck,
        bodies: Option<HashMap<(usize, usize), crate::eval::Elaborated>>,
    ) -> Self {
        let mut out = CheckedProgram::new();
        for index in 0..ws.modules.len() {
            let loaded = &ws.modules[index];
            let mut module = CheckedModule::new(index, loaded.path.display().to_string());
            for (def, d) in loaded.module.defs.iter().enumerate() {
                let holes = tc.holes(index, def);
                // Only well-typed definitions get an entry, which is what the
                // field's contract says. This used to insert `""` for a
                // definition that failed to check, so a caller could not tell
                // "has no type" from "its type printed as the empty string" —
                // and `""` is a plausible-looking value a consumer might go on
                // to display. Absence is the honest encoding.
                if let Some(t) = tc.type_of(index, def) {
                    module.types.insert(def, t.to_string());
                }
                module.holes.insert(def, holes);
                // The printed type of every name use, keyed by span: this map
                // really is span-keyed (`TypeCheck::use_types`), so a span
                // lookup is correct here.
                for (span, _layer) in body_spans(&d.body) {
                    if let Some(t) = tc.use_type(index, span) {
                        module.use_types.insert((span.start, span.end), t.to_string());
                    }
                }
                // Overload choices are keyed by `ExprId`, NOT by span. This
                // used to look them up with `span.start`/`span.end`; those are
                // a different namespace, so a definition with overloads
                // transferred *no* choices, and — both being small integers —
                // could transfer another use's choice when the ranges
                // happened to overlap. `choices_of` reads the pairs the
                // checker recorded, so the key cannot be wrong.
                for (site, k, c) in tc.choices_of(index, def) {
                    module.choices.insert((def, site, k), c);
                }
                module.defs.push(CheckedDef {
                    name: d.name.clone(),
                    def,
                    scheme: tc.type_of(index, def).map(str::to_string),
                    // The typed twin of `scheme`: read from the same
                    // `TypeCheck`, so `CheckedProgram` and the checker cannot
                    // disagree about a definition's type.
                    scalar: tc.def_scalar(index, def),
                    row: scheme_row(tc, index, def),
                    holes,
                    terms: bodies
                        .as_ref()
                        .and_then(|b| b.get(&(index, def)).cloned()),
                });
            }
            if let Some(fs) = tc.probe_fields(index) {
                module.probe_fields = Some(fs.to_vec());
            }
            out.modules.push(module);
        }
        for e in &tc.errors {
            out.diagnostics
                .push(Diagnostic::new(e.module, Some(e.def), e.diag.clone()));
        }
        for d in &ws.diags {
            out.diagnostics.push(Diagnostic::new(ws.root, None, d.clone()));
        }
        out
    }

    pub fn module(&self, index: usize) -> Option<&CheckedModule> {
        self.modules.iter().find(|m| m.index == index)
    }

    /// True when nothing was reported. A program with errors still carries
    /// whatever modules were checked, so a language server can show both.
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

impl Default for CheckedProgram {
    fn default() -> Self {
        Self::new()
    }
}


/// A definition's output row, from its scheme's closed row.
///
/// `None` means the scheme is not a query over a closed row — a function, a
/// scalar, or a row the checker never closed. That is deliberately distinct
/// from `Some(RowType::new(vec![]))`, a query over no columns: collapsing them
/// would make "this definition has no row" and "this definition's row is
/// empty" the same answer, and they are not the same program.
///
/// The classification is structural (`TypeCheck::scheme_view`), computed from
/// the type. An earlier version tested the *printed* scheme for the word
/// `query`, which also matches a function that merely mentions a query:
/// `f = q => q & where (.id > 0)` was reported as a query with no columns
/// instead of a function.
fn scheme_row(tc: &TypeCheck, module: usize, def: usize) -> Option<RowType> {
    match tc.scheme_view(module, def)? {
        SchemeView::Query { fields } => fields.map(RowType::new),
        // A bare row type (`{ a = int }`) is a row, but not a query; callers
        // that want a query's columns should not get a bare row's.
        SchemeView::Row { .. } | SchemeView::Scalar(_) | SchemeView::Function | SchemeView::Open => {
            None
        }
    }
}

/// Every sub-expression span of a definition body, the body's own first.
///
/// `TypeCheck::use_type` and `TypeCheck::choice` are keyed by span and use
/// site, so reading them requires walking the body.
fn body_spans(e: &ast::Expr) -> Vec<(ast::Span, ())> {
    let mut out = vec![(e.span, ())];
    walk(e, &mut out);
    out
}

fn walk(e: &ast::Expr, out: &mut Vec<(ast::Span, ())>) {
    match &e.kind {
        ast::ExprKind::Name(_)
        | ast::ExprKind::Lit(_)
        | ast::ExprKind::Field(..)
        | ast::ExprKind::Sql(_)
        | ast::ExprKind::Primitive(_)
        | ast::ExprKind::Error => {}
        ast::ExprKind::Proj(b, _) => walk(b, out),
        ast::ExprKind::App(f, args) => {
            walk(f, out);
            for a in args {
                walk(a, out);
            }
        }
        ast::ExprKind::Lambda(_, b) => walk(b, out),
        ast::ExprKind::Record(fs) => {
            for (_, x) in fs {
                walk(x, out);
            }
        }
        ast::ExprKind::List(xs) => {
            for x in xs {
                walk(x, out);
            }
        }
    }
    out.push((e.span, ()));
}

// ── the checked query ──────────────────────────────────────────────────────

/// Which stage produced a [`CheckedQuery`].
///
/// The node enum itself is crate-private, so this is how a consumer outside
/// the crate branches on a query's shape without being able to build one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryKind {
    Table,
    Where,
    Select,
    Update,
    Omit,
    /// `prefix` or `suffix`.
    Rename,
    Agg,
    Order,
    Limit,
    Offset,
    Distinct,
    Join,
    Set,
}

/// Which expression form produced a [`CheckedExpr`]; see [`QueryKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExprKind {
    /// A column reference (`.x`, `.<x`, `.>x`).
    Column,
    /// A scalar literal.
    Lit,
    /// A scalar SQL template (`sql "..."`).
    Template,
    /// An aggregate template (`agg`).
    AggTemplate,
    /// A window template (`win`).
    WinTemplate,
    /// `in`.
    In,
    /// `group`.
    Group,
    /// A call still to be inlined by elaboration.
    Call,
}

/// A checked query: an output row, a node that produced it, and where the
/// stage was written.
///
/// The fields are private, and no public constructor takes a `row` and a
/// `node` separately. That is the point of the type: each constructor below
/// *checks* its stage's rule before assembling these three fields, so a
/// `CheckedQuery` that exists has a `row` its `node` actually produces. A
/// public field would let a caller pair any row with any node and silently
/// falsify every downstream guarantee, including the `erase`/`schema`
/// invariant the tests assert.
///
/// Read access is through [`CheckedQuery::row`], [`CheckedQuery::node`] and
/// [`CheckedQuery::origin`]. `node` returns the crate-private
/// `CheckedQueryNode`, so a consumer outside this crate can inspect a shape
/// but cannot construct one — which is what makes "the constructors are the
/// only way" a fact rather than a convention.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedQuery {
    row: RowType,
    node: CheckedQueryNode,
    origin: Origin,
}

impl CheckedQuery {
    /// The row this query produces: ordered `(column, type)` pairs.
    pub fn row(&self) -> &RowType {
        &self.row
    }

    /// Where the stage was written.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Consume this query, yielding its parts.
    ///
    /// Crate-private: only the erasure path (`core_term::of_checked`,
    /// `erase`) needs to take a `CheckedQuery` apart by value, and it lives in
    /// this crate. Keeping it `pub(crate)` means the *public* surface never
    /// hands out a `CheckedQueryNode` that a caller could reassemble.
    pub(crate) fn into_parts(self) -> (RowType, CheckedQueryNode, Origin) {
        (self.row, self.node, self.origin)
    }

    /// Which stage built this query, without exposing the node type.
    ///
    /// A consumer outside the crate that needs to branch on the shape reads
    /// this rather than matching the node: the node enum is `pub(crate)`
    /// precisely so that a caller cannot construct one and bypass the
    /// constructors' checks.
    pub fn kind(&self) -> QueryKind {
        match &self.node {
            CheckedQueryNode::Table { .. } => QueryKind::Table,
            CheckedQueryNode::Where { .. } => QueryKind::Where,
            CheckedQueryNode::Select { .. } => QueryKind::Select,
            CheckedQueryNode::Update { .. } => QueryKind::Update,
            CheckedQueryNode::Omit { .. } => QueryKind::Omit,
            CheckedQueryNode::Rename { .. } => QueryKind::Rename,
            CheckedQueryNode::Agg { .. } => QueryKind::Agg,
            CheckedQueryNode::Order { .. } => QueryKind::Order,
            CheckedQueryNode::Limit { .. } => QueryKind::Limit,
            CheckedQueryNode::Offset { .. } => QueryKind::Offset,
            CheckedQueryNode::Distinct { .. } => QueryKind::Distinct,
            CheckedQueryNode::Join { .. } => QueryKind::Join,
            CheckedQueryNode::Set { .. } => QueryKind::Set,
        }
    }
    /// A table, with its columns known.
    ///
    /// `columns` is the table's closed row: the checker gives a `table`
    /// definition its column list from its signature, and a table whose
    /// columns are not known cannot be a checked query — every later stage
    /// would have to guess. `None` is rejected here rather than deferred, with
    /// the same message `schema` gives.
    pub fn table(
        schema: impl Into<String>,
        name: impl Into<String>,
        columns: Option<RowType>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let schema = schema.into();
        let name = name.into();
        let Some(row) = columns else {
            return Err(Error::new(format!(
                "the columns of table `{schema}.{name}` are unknown; give its definition a closed \
                 type, e.g. `t : query {{ id = int }} = table \"{schema}\" \"{name}\"`"
            ))
            .at(origin));
        };
        // Read the names before `row` is moved into the node.
        let names = row.names();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Table {
                schema,
                name,
                columns: names,
            },
            origin,
        })
    }

    /// `where pred`: a row-phase `bool` predicate; the output row is the
    /// input's.
    pub fn where_(input: CheckedQuery, pred: CheckedExpr, origin: Origin) -> Result<Self, Error> {
        place(Place::Where, &pred, origin)?;
        if !pred.ty.is_bool() {
            return Err(Error::new(format!(
                "a `where` predicate must be bool, found {}",
                pred.ty
            ))
            .at(origin));
        }
        refs_columns(&pred, &input.row, "where", origin)?;
        let row = input.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Where {
                input: Box::new(input),
                pred,
            },
            origin,
        })
    }

    /// `select {..}`: at least one field, every field row-phase and over the
    /// input row; the output row is the fields, in order.
    pub fn select(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let row = project_row("select", &input.row, &fields, Place::Select, origin)?;
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Select {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `update {..}`: like `select`, but merged over the input row — a listed
    /// name keeps its position and takes the **new** type, a new name is
    /// appended.
    ///
    /// The merge here is [`RowType::overwrite`] (right-wins), *not*
    /// [`RowType::merge`] (left-wins, the join law). Using the join law would
    /// silently keep the old type of an overwritten column while the checker
    /// and the erasure both say the new one.
    pub fn update(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let updated = project_row("update", &input.row, &fields, update_place(), origin)?;
        let row = input.row.overwrite(&updated);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Update {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `agg {..}`: at least one field, every field at `Agg` or `Const` phase.
    /// A `Win` field is rejected: it belongs in a `select` after the
    /// aggregation.
    pub fn agg(
        input: CheckedQuery,
        fields: Vec<(String, CheckedExpr)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        let row = project_row("agg", &input.row, &fields, Place::Agg, origin)?;
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Agg {
                input: Box::new(input),
                fields,
            },
            origin,
        })
    }

    /// `order [..]`: row-phase sort keys over the input row; the output row
    /// is unchanged.
    pub fn order(
        input: CheckedQuery,
        keys: Vec<(CheckedExpr, bool)>,
        origin: Origin,
    ) -> Result<Self, Error> {
        for (k, _) in &keys {
            place(Place::Key, k, origin)?;
            refs_columns(k, &input.row, "order", origin)?;
        }
        let row = input.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Order {
                input: Box::new(input),
                keys,
            },
            origin,
        })
    }

    /// `limit n`.
    pub fn limit(input: CheckedQuery, n: i64, origin: Origin) -> Result<Self, Error> {
        if n < 0 {
            return Err(
                Error::new(format!("`limit` needs a non-negative count, got {n}")).at(origin)
            );
        }
        let row = input.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Limit {
                input: Box::new(input),
                n,
            },
            origin,
        })
    }

    /// `offset n`.
    pub fn offset(input: CheckedQuery, n: i64, origin: Origin) -> Result<Self, Error> {
        if n < 0 {
            return Err(
                Error::new(format!("`offset` needs a non-negative count, got {n}")).at(origin)
            );
        }
        let row = input.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Offset {
                input: Box::new(input),
                n,
            },
            origin,
        })
    }

    /// `distinct`.
    pub fn distinct(input: CheckedQuery, origin: Origin) -> Result<Self, Error> {
        let row = input.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Distinct(Box::new(input)),
            origin,
        })
    }

    /// `omit "k"`: the input row without column `k`.
    ///
    /// A key the input does not have is **rejected here**, with the wording
    /// `schema::omit_columns` uses. [`RowType::omit`] is total because it is
    /// the row *former*, but this constructor is where the rule lives: a
    /// `CheckedQuery` that exists must be one `schema::schema` accepts, and
    /// `omit` on a missing key is exactly the case where a total row former
    /// would otherwise let an invalid query into the checked layer.
    pub fn omit(input: CheckedQuery, key: impl Into<String>, origin: Origin) -> Result<Self, Error> {
        let key = key.into();
        if !input.row.has(&key) {
            let avail = input.row.names().join(", ");
            return Err(Error::new(format!(
                "no column `{key}`; available: {avail}"
            ))
            .at(origin));
        }
        let row = input.row.omit(&key);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Omit {
                input: Box::new(input),
                key,
            },
            origin,
        })
    }

    /// `prefix "s"` — every column name gains `affix` in front.
    pub fn prefix(input: CheckedQuery, affix: impl Into<String>, origin: Origin) -> Result<Self, Error> {
        Self::rename(input, affix, true, origin)
    }

    /// `suffix "s"` — every column name gains `affix` at the end.
    pub fn suffix(input: CheckedQuery, affix: impl Into<String>, origin: Origin) -> Result<Self, Error> {
        Self::rename(input, affix, false, origin)
    }

    /// The shared body of `prefix`/`suffix`. It keeps the direction explicit
    /// so a stale mapper cannot make `suffix` act as a prefix.
    pub fn rename(
        input: CheckedQuery,
        affix: impl Into<String>,
        prefix: bool,
        origin: Origin,
    ) -> Result<Self, Error> {
        let affix = affix.into();
        let row = input.row.rename_all(&affix, prefix);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Rename {
                input: Box::new(input),
                affix,
                prefix,
            },
            origin,
        })
    }

    /// A join.
    ///
    /// The predicate must be row-phase `bool`, and every side-qualified name
    /// must come from the input its side names: `Side::Single` is rejected (a
    /// join predicate must say which input a column comes from), and a column
    /// absent from its side is reported with what that side has.
    ///
    /// The output row is the left input's, then the right's that the left does
    /// not have, with the nullable side's *types* wrapped in `maybe`: left join
    /// → right side, right join → left side, full → both. A semi/anti join
    /// keeps the left input's row.
    pub fn join(
        kind: JoinKind,
        left: CheckedQuery,
        right: CheckedQuery,
        on: CheckedExpr,
        origin: Origin,
    ) -> Result<Self, Error> {
        place(Place::JoinOn, &on, origin)?;
        if !on.ty.is_bool() {
            return Err(Error::new(format!(
                "a join predicate must be bool, found {}",
                on.ty
            ))
            .at(origin));
        }
        for (side, n) in on.node.columns() {
            let (cols, what) = match side {
                Side::Left => (&left.row, "left"),
                Side::Right => (&right.row, "right"),
                Side::Single => return Err(Error::new(rules::needs_side(&n)).at(origin)),
            };
            if !cols.has(&n) {
                let avail = cols.names().join(", ");
                return Err(Error::new(format!(
                    "the {what} join input has no column `{n}`; available: {avail}"
                ))
                .at(origin));
            }
        }
        let row = join_row(kind, &left.row, &right.row);
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Join {
                kind,
                left: Box::new(left),
                right: Box::new(right),
                on,
            },
            origin,
        })
    }

    /// A set operation: both inputs must expose the same row — the same names,
    /// in the same order, with the same types.
    pub fn set(
        kind: SetKind,
        left: CheckedQuery,
        right: CheckedQuery,
        origin: Origin,
    ) -> Result<Self, Error> {
        if left.row.names() != right.row.names() {
            return Err(Error::new(format!(
                "set-operation inputs must have the same columns; left has [{}], right has [{}]",
                left.row.names().join(", "),
                right.row.names().join(", ")
            ))
            .at(origin));
        }
        if left.row.columns != right.row.columns {
            let differing = left
                .row
                .columns()
                .iter()
                .zip(right.row.columns())
                .find(|(l, r)| l != r)
                .map(|(l, r)| format!("column `{}` is {} on the left but {} on the right", l.0, l.1, r.1))
                .unwrap_or_else(|| "the two inputs differ".into());
            return Err(Error::new(format!(
                "set-operation inputs must have the same column types; {differing}"
            ))
            .at(origin));
        }
        let row = left.row.clone();
        Ok(CheckedQuery {
            row,
            node: CheckedQueryNode::Set {
                kind,
                left: Box::new(left),
                right: Box::new(right),
            },
            origin,
        })
    }

    /// The query's output row as a plain column list, for a comparison
    /// against `schema::schema(&erase(q))`.
    pub fn names(&self) -> Vec<String> {
        self.row.names()
    }

    /// Lower this query to the current relational IR (see [`erase`]).
    ///
    /// Infallible because a `CheckedQuery` has no expression in it yet: every
    /// `CheckedExpr` is attached by a node's fields, and this walks them. Use
    /// [`erase`] directly if you need the `Result`.
    ///
    /// # Panics
    ///
    /// Only if a `CheckedExprNode::Call` was reached, which is a compiler bug:
    /// calls are meant to be inlined or reduced before erasure. `erase` returns
    /// the error instead.
    pub fn erase(self) -> Rel {
        erase(self).unwrap_or_else(|e| panic!("internal error: {e}"))
    }

    /// `schema::schema` run over this query's erasure.
    pub fn erased_schema(&self) -> Result<Vec<String>, String> {
        crate::schema::schema(&erase(self.clone()).map_err(|e| e.message)?)
    }
}

/// The output row of a join, per kind. `join_columns` (via `RowType::merge`)
/// is the same left-wins rule `schema` uses, and the nullable side's types go
/// through `map_value_nullable`, the `mapValue (AsNullable)` term the checker
/// builds.
pub fn join_row(kind: JoinKind, left: &RowType, right: &RowType) -> RowType {
    match kind {
        JoinKind::Semi | JoinKind::Anti => left.clone(),
        JoinKind::Inner => left.merge(right),
        JoinKind::Left => left.merge(&right.map_value_nullable()),
        JoinKind::Right => left.map_value_nullable().merge(right),
        JoinKind::Full => left
            .map_value_nullable()
            .merge(&right.map_value_nullable()),
    }
}

/// A checked query's node. Each variant mirrors the `Rel` variant of the same
/// name, plus the row it produces (carried by [`CheckedQuery`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckedQueryNode {
    Table {
        schema: String,
        name: String,
        columns: Vec<String>,
    },
    Where {
        input: Box<CheckedQuery>,
        pred: CheckedExpr,
    },
    Select {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Update {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Omit {
        input: Box<CheckedQuery>,
        key: String,
    },
    /// `prefix`/`suffix`: one node with the direction, because both erase to a
    /// rename and nothing downstream cares which spelling produced it.
    Rename {
        input: Box<CheckedQuery>,
        affix: String,
        prefix: bool,
    },
    Agg {
        input: Box<CheckedQuery>,
        fields: Vec<(String, CheckedExpr)>,
    },
    Order {
        input: Box<CheckedQuery>,
        keys: Vec<(CheckedExpr, bool)>,
    },
    Limit {
        input: Box<CheckedQuery>,
        n: i64,
    },
    Offset {
        input: Box<CheckedQuery>,
        n: i64,
    },
    Distinct(Box<CheckedQuery>),
    Join {
        kind: JoinKind,
        left: Box<CheckedQuery>,
        right: Box<CheckedQuery>,
        on: CheckedExpr,
    },
    Set {
        kind: SetKind,
        left: Box<CheckedQuery>,
        right: Box<CheckedQuery>,
    },
}

impl CheckedQueryNode {
    #[cfg(test)]
    /// The single input of a unary stage, if it has one.
    pub(crate) fn input(&self) -> Option<&CheckedQuery> {
        match self {
            CheckedQueryNode::Table { .. } => None,
            CheckedQueryNode::Where { input, .. }
            | CheckedQueryNode::Select { input, .. }
            | CheckedQueryNode::Update { input, .. }
            | CheckedQueryNode::Omit { input, .. }
            | CheckedQueryNode::Rename { input, .. }
            | CheckedQueryNode::Agg { input, .. }
            | CheckedQueryNode::Order { input, .. }
            | CheckedQueryNode::Limit { input, .. }
            | CheckedQueryNode::Offset { input, .. }
            | CheckedQueryNode::Distinct(input) => Some(input),
            CheckedQueryNode::Join { left, .. } | CheckedQueryNode::Set { left, .. } => Some(left),
        }
    }

    /// The expressions this node reads directly (not through an input).
    ///
    /// A test affordance: production code walks the node variants directly on
    /// the erasure path.
    #[cfg(test)]
    pub(crate) fn exprs(&self) -> Vec<&CheckedExpr> {
        match self {
            CheckedQueryNode::Table { .. }
            | CheckedQueryNode::Limit { .. }
            | CheckedQueryNode::Offset { .. }
            | CheckedQueryNode::Omit { .. }
            | CheckedQueryNode::Rename { .. }
            | CheckedQueryNode::Distinct(_)
            | CheckedQueryNode::Set { .. } => vec![],
            CheckedQueryNode::Where { pred, .. } => vec![pred],
            CheckedQueryNode::Select { fields, .. }
            | CheckedQueryNode::Update { fields, .. }
            | CheckedQueryNode::Agg { fields, .. } => fields.iter().map(|(_, e)| e).collect(),
            CheckedQueryNode::Order { keys, .. } => keys.iter().map(|(e, _)| e).collect(),
            CheckedQueryNode::Join { on, .. } => vec![on],
        }
    }

    /// Every side-qualified column reference in this node's own expressions.
    #[cfg(test)]
    pub(crate) fn columns(&self) -> Vec<(Side, String)> {
        self.exprs().into_iter().flat_map(|e| e.node.columns()).collect()
    }
}

// ── the checked expression ─────────────────────────────────────────────────

/// A checked expression: its phase, its scalar type, its node, its origin.
///
/// Private fields for the same reason as [`CheckedQuery`]: the phase, the
/// scalar type and the node have to agree — an `agg`-phase node carrying a
/// `row` phase would place an aggregate where SQL rejects one — and the
/// constructors are what establish that agreement. Use [`CheckedExpr::phase`],
/// [`CheckedExpr::ty`], [`CheckedExpr::node`] and [`CheckedExpr::origin`].
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedExpr {
    phase: Phase,
    ty: ScalarType,
    node: CheckedExprNode,
    origin: Origin,
}

impl CheckedExpr {
    /// The phase this expression sits in (`const`, `row`, `agg`, `win`).
    ///
    /// Public because it is part of what a consumer needs to know about an
    /// expression; the *node* stays private, so the phase cannot be paired
    /// with a node that contradicts it.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The scalar type this expression has, for a consumer that needs it
    /// without the node.
    pub fn ty(&self) -> &ScalarType {
        &self.ty
    }

    /// Where the expression was written.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Consume this expression, yielding its parts; crate-private, as with
    /// [`CheckedQuery::into_parts`].
    pub(crate) fn into_parts(self) -> (Phase, ScalarType, CheckedExprNode, Origin) {
        (self.phase, self.ty, self.node, self.origin)
    }

    /// Which expression form built this, without exposing the node type.
    pub fn kind(&self) -> ExprKind {
        match &self.node {
            CheckedExprNode::Column { .. } => ExprKind::Column,
            CheckedExprNode::Lit(_) => ExprKind::Lit,
            CheckedExprNode::Template { .. } => ExprKind::Template,
            CheckedExprNode::AggTemplate { .. } => ExprKind::AggTemplate,
            CheckedExprNode::WinTemplate { .. } => ExprKind::WinTemplate,
            CheckedExprNode::In { .. } => ExprKind::In,
            CheckedExprNode::Group(_) => ExprKind::Group,
            CheckedExprNode::Call { .. } => ExprKind::Call,
        }
    }
    /// A column reference (`.x`, `.<x`, `.>x`). Row phase; the type is the one
    /// the checker gave the column.
    pub fn column(side: Side, name: impl Into<String>, ty: ScalarType, origin: Origin) -> Self {
        CheckedExpr {
            phase: Phase::Row,
            ty,
            node: CheckedExprNode::Column {
                side,
                name: name.into(),
            },
            origin,
        }
    }

    /// A literal. `Const` phase, with the literal's own type.
    pub fn lit(lit: Lit, origin: Origin) -> Self {
        let ty = match &lit {
            Lit::Int(_) => ScalarType::Int,
            Lit::Float(_) => ScalarType::Float,
            Lit::Str(_) => ScalarType::String,
            Lit::Bool(_) => ScalarType::Bool,
        };
        CheckedExpr {
            phase: Phase::Const,
            ty,
            node: CheckedExprNode::Lit(lit),
            origin,
        }
    }

    /// A scalar SQL template: its phase is the [`rules::mix`] of its
    /// arguments', so a template over row arguments is row phase and one over
    /// aggregate arguments is an aggregate.
    pub fn template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        let phase = mix_of(&args, origin)?;
        Ok(CheckedExpr {
            phase,
            ty,
            node: CheckedExprNode::Template {
                sql: sql.into(),
                args,
            },
            origin,
        })
    }

    /// An aggregate template: every argument must be row phase (the depth-1
    /// rule — aggregates do not nest). The result is `Agg` phase whatever the
    /// arguments' own phases were, as long as they were row/const.
    pub fn agg_template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        row_args("an aggregate argument", &args, origin)?;
        Ok(CheckedExpr {
            phase: Phase::Agg,
            ty,
            node: CheckedExprNode::AggTemplate {
                sql: sql.into(),
                args,
            },
            origin,
        })
    }

    /// A window template: every argument and every window-spec expression must
    /// be row phase.
    pub fn win_template(
        sql: impl Into<String>,
        args: Vec<CheckedExpr>,
        spec: WinSpecChecked,
        ty: ScalarType,
        origin: Origin,
    ) -> Result<Self, Error> {
        row_args("a window argument", &args, origin)?;
        spec.check()?;
        Ok(CheckedExpr {
            phase: Phase::Win,
            ty,
            node: CheckedExprNode::WinTemplate {
                sql: sql.into(),
                args,
                spec,
            },
            origin,
        })
    }

    /// `group e`: the key must be row phase; the result is an aggregate.
    pub fn group(key: CheckedExpr, origin: Origin) -> Result<Self, Error> {
        row_phase("a group key", &key, origin)?;
        Ok(CheckedExpr {
            phase: Phase::Agg,
            ty: key.ty.clone(),
            node: CheckedExprNode::Group(Box::new(key)),
            origin,
        })
    }

    /// `in`: value and list mix per [`rules::mix`]; the result is bool at the
    /// mixed phase.
    pub fn in_(
        value: CheckedExpr,
        list: Vec<CheckedExpr>,
        negated: bool,
        origin: Origin,
    ) -> Result<Self, Error> {
        let mut phase = value.phase;
        for e in &list {
            phase = rules::mix(phase, e.phase).map_err(|m| Error::new(m).at(e.origin))?;
        }
        let elem = value.ty.clone();
        Ok(CheckedExpr {
            phase,
            ty: ScalarType::Bool,
            node: CheckedExprNode::In {
                value: Box::new(value),
                list,
                negated,
                elem,
            },
            origin,
        })
    }

    /// A use of a checked definition, with the instantiation chosen for its
    /// overload holes. This is what makes an overload choice explicit at the
    /// call site instead of a dynamic lookup during evaluation.
    pub fn call(
        name: impl Into<String>,
        def: (usize, usize),
        holes: Vec<Choice>,
        ty: ScalarType,
        origin: Origin,
    ) -> Self {
        CheckedExpr {
            phase: Phase::Row,
            ty,
            node: CheckedExprNode::Call {
                name: name.into(),
                def,
                holes,
            },
            origin,
        }
    }
}

/// A checked expression's node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckedExprNode {
    Column {
        side: Side,
        name: String,
    },
    Lit(Lit),
    Template {
        sql: String,
        args: Vec<CheckedExpr>,
    },
    AggTemplate {
        sql: String,
        args: Vec<CheckedExpr>,
    },
    WinTemplate {
        sql: String,
        args: Vec<CheckedExpr>,
        spec: WinSpecChecked,
    },
    In {
        value: Box<CheckedExpr>,
        list: Vec<CheckedExpr>,
        negated: bool,
        /// The value's type, resolved once here so `erase` does not look at
        /// the elements again.
        elem: ScalarType,
    },
    Group(Box<CheckedExpr>),
    /// A checked definition used as an expression: the callee's name and the
    /// instantiation chosen for its holes.
    Call {
        name: String,
        def: (usize, usize),
        holes: Vec<Choice>,
    },
}

impl CheckedExprNode {
    /// Every column this expression reads, in reading order.
    ///
    /// Production: the join constructor and the column-reference check both
    /// need it to test a predicate's names against the input rows.
    pub(crate) fn columns(&self) -> Vec<(Side, String)> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut Vec<(Side, String)>) {
        match self {
            CheckedExprNode::Column { side, name } => out.push((*side, name.clone())),
            CheckedExprNode::Lit(_) | CheckedExprNode::Call { .. } => {}
            CheckedExprNode::Template { args, .. } | CheckedExprNode::AggTemplate { args, .. } => {
                for a in args {
                    a.node.collect_columns(out);
                }
            }
            CheckedExprNode::WinTemplate { args, spec, .. } => {
                for a in args {
                    a.node.collect_columns(out);
                }
                spec.collect_columns(out);
            }
            CheckedExprNode::In { value, list, .. } => {
                for a in list.iter().chain(std::iter::once(value.as_ref())) {
                    a.node.collect_columns(out);
                }
            }
            CheckedExprNode::Group(k) => k.node.collect_columns(out),
        }
    }

}

/// A checked window specification.
#[derive(Debug, Clone, PartialEq)]
pub struct WinSpecChecked {
    pub partition: Vec<CheckedExpr>,
    pub order: Vec<(CheckedExpr, bool)>,
    pub frame: Option<Frame>,
}

impl WinSpecChecked {
    /// Every partition and order expression must satisfy the key rule.
    pub fn check(&self) -> Result<(), Error> {
        for e in self.partition.iter() {
            place(Place::Key, e, e.origin)?;
        }
        for (e, _) in &self.order {
            place(Place::Key, e, e.origin)?;
        }
        Ok(())
    }

    fn collect_columns(&self, out: &mut Vec<(Side, String)>) {
        for e in self.partition.iter() {
            e.node.collect_columns(out);
        }
        for (e, _) in &self.order {
            e.node.collect_columns(out);
        }
    }
}

/// A window spec from its parts, checking the frame.
pub fn window_spec(
    partition: Vec<CheckedExpr>,
    order: Vec<(CheckedExpr, bool)>,
    frame: Option<Frame>,
) -> Result<WinSpecChecked, Error> {
    let spec = WinSpecChecked {
        partition,
        order,
        frame,
    };
    spec.check()?;
    Ok(spec)
}

/// A `ROWS BETWEEN` frame from two bounds, rejecting impossible ones (the
/// check `prims::call` does for `__rows`).
pub fn frame(start: Bound, end: Bound) -> Result<Frame, Error> {
    if start == Bound::UnboundedFollowing
        || end == Bound::UnboundedPreceding
        || start.key() > end.key()
    {
        return Err(Error::new(format!(
            "impossible window frame: {start:?} to {end:?}"
        )));
    }
    Ok(Frame { start, end })
}

// ── the shared rules ───────────────────────────────────────────────────────

/// A node's phase from its arguments, via the existing mix rule.
fn mix_of(args: &[CheckedExpr], origin: Origin) -> Result<Phase, Error> {
    args.iter().try_fold(Phase::Const, |acc, a| {
        rules::mix(acc, a.phase)
            .map_err(|m| Error::new(m).at(a.origin))
            .map_err(|e| e.at(origin))
    })
}

/// The existing placement rule, pointed at the offending expression.
fn place(place: Place, e: &CheckedExpr, origin: Origin) -> Result<(), Error> {
    rules::place(place, e.phase)
        .map_err(|m| Error::new(m).at(e.origin))
        .map_err(|err| err.at(origin))
}

/// The existing depth-1 rule, pointed at the offending expression.
fn row_phase(what: &str, e: &CheckedExpr, origin: Origin) -> Result<(), Error> {
    rules::nested(what, e.phase)
        .map_err(|m| Error::new(m).at(e.origin))
        .map_err(|err| err.at(origin))
}

fn row_args(what: &str, args: &[CheckedExpr], origin: Origin) -> Result<(), Error> {
    for a in args {
        row_phase(what, a, origin)?;
    }
    Ok(())
}

/// Every plain column reference must be a column of `input`; a side-qualified
/// one is a join-predicate form and is rejected outside a join
/// (`rules::JOIN_ONLY`, the same message `schema::refs` gives).
fn refs_columns(e: &CheckedExpr, input: &RowType, ctx: &str, origin: Origin) -> Result<(), Error> {
    for (side, n) in e.node.columns() {
        match side {
            Side::Single if !input.has(&n) => {
                let avail = input.names().join(", ");
                return Err(Error::new(format!(
                    "no column `{n}` in the input of `{ctx}`; available: {avail}"
                ))
                .at(origin));
            }
            Side::Left | Side::Right => return Err(Error::new(rules::JOIN_ONLY).at(origin)),
            _ => {}
        }
    }
    Ok(())
}

/// The output row of `select`/`update`/`agg`: at least one field, each field
/// placed by `at`, each referring to `input`, labelled with its own type.
///
/// `at` is `Place::Select` for `select`/`update` and `Place::Agg` for `agg`;
/// the message for a rejected field is the one the checker's `stage_phase`
/// produces, so a user sees one explanation.
fn project_row(
    stage: &str,
    input: &RowType,
    fields: &[(String, CheckedExpr)],
    at: Place,
    origin: Origin,
) -> Result<RowType, Error> {
    if fields.is_empty() {
        return Err(Error::new(format!("`{stage}` needs at least one field")).at(origin));
    }
    // A repeated field is reported before anything else, with the wording the
    // checker uses per stage (`check/infer.rs` `Cons::Update`: "field `n`
    // appears twice in `update`"; `Cons::Project` runs the same rule for
    // `select`/`agg`), so a user sees one explanation rather than two.
    for (i, (n, _)) in fields.iter().enumerate() {
        if fields[..i].iter().any(|(o, _)| o == n) {
            return Err(Error::new(format!("field `{n}` appears twice in `{stage}`")).at(origin));
        }
    }
    let mut out = Vec::with_capacity(fields.len());
    for (n, e) in fields {
        refs_columns(e, input, stage, origin)
            .map_err(|err| Error::new(format!("field `{n}`: {}", err.message)).at(e.origin))?;
        rules::place(at, e.phase)
            .map_err(|m| Error::new(format!("field `{n}` {m}")).at(e.origin))?;
        out.push((n.clone(), e.ty.clone()));
    }
    Ok(RowType::project(&out))
}

/// `update`'s fields are placed like `select`'s: row phase, over the input.
fn update_place() -> Place {
    Place::Select
}

// ── erasure to the existing IR ─────────────────────────────────────────────

/// Lower a checked query to the current relational IR.
///
/// Structural: every constructor that returned `Ok` erases to the `Rel`
/// today's evaluator builds for the same program, and the erased tree is what
/// `schema::schema` and the SQL backend already accept.
///
/// Fallible, but only through [`erase_expr`]: a `CheckedExprNode::Call` has no
/// SQL expression, so a query containing one cannot be erased. No *query* node
/// can fail — the query side is total by construction, which is what the
/// `Result` makes visible rather than assumed.
///
/// Each node is stamped with `Rel::At(origin)`, except where the node is
/// already an `At` — the evaluator's existing convention, so a tree that came
/// from evaluation is not wrapped twice.
pub fn erase(query: CheckedQuery) -> Result<Rel, Error> {
    let (row, node, origin) = query.into_parts();
    let _ = row;
    let loc = Loc {
        module: origin.module,
        span: origin.span,
    };
    let at = |r: Rel| match r {
        r @ Rel::At(..) => r,
        r => Rel::At(loc, Box::new(r)),
    };
    let rel = match node {
        CheckedQueryNode::Table {
            schema,
            name,
            columns,
        } => at(Rel::Table {
            schema,
            name,
            columns: Some(columns),
        }),
        CheckedQueryNode::Where { input, pred } => {
            at(Rel::Where(Box::new(erase(*input)?), erase_expr(pred)?))
        }
        CheckedQueryNode::Select { input, fields } => at(Rel::Select(
            Box::new(erase(*input)?),
            erase_fields(fields)?,
        )),
        CheckedQueryNode::Update { input, fields } => at(Rel::Update(
            Box::new(erase(*input)?),
            erase_fields(fields)?,
        )),
        CheckedQueryNode::Omit { input, key } => at(Rel::Omit(Box::new(erase(*input)?), key)),
        CheckedQueryNode::Rename {
            input,
            affix,
            prefix,
        } => {
            let inner = Box::new(erase(*input)?);
            at(if prefix {
                Rel::Prefix(inner, affix)
            } else {
                Rel::Suffix(inner, affix)
            })
        }
        CheckedQueryNode::Agg { input, fields } => at(Rel::Agg(
            Box::new(erase(*input)?),
            erase_fields(fields)?,
        )),
        CheckedQueryNode::Order { input, keys } => at(Rel::Order(
            Box::new(erase(*input)?),
            keys.into_iter()
                .map(|(e, asc)| Ok((erase_expr(e)?, asc)))
                .collect::<Result<Vec<_>, Error>>()?,
        )),
        CheckedQueryNode::Limit { input, n } => at(Rel::Limit(Box::new(erase(*input)?), n)),
        CheckedQueryNode::Offset { input, n } => at(Rel::Offset(Box::new(erase(*input)?), n)),
        CheckedQueryNode::Distinct(input) => at(Rel::Distinct(Box::new(erase(*input)?))),
        CheckedQueryNode::Join {
            kind,
            left,
            right,
            on,
        } => at(Rel::Join {
            kind,
            left: Box::new(erase(*left)?),
            right: Box::new(erase(*right)?),
            on: erase_expr(on)?,
        }),
        CheckedQueryNode::Set { kind, left, right } => at(Rel::Set {
            kind,
            left: Box::new(erase(*left)?),
            right: Box::new(erase(*right)?),
        }),
    };
    Ok(rel)
}

/// Erase a list of checked expressions (a projection's fields), stopping at the
/// first that cannot be erased.
fn erase_fields(
    fields: Vec<(String, CheckedExpr)>,
) -> Result<Vec<(String, Expr)>, Error> {
    fields
        .into_iter()
        .map(|(n, e)| Ok((n, erase_expr(e)?)))
        .collect()
}

/// Lower a checked expression to the current `Expr`.
///
/// Fallible for the same reason as [`core_term::erase_core`]: a
/// `CheckedExprNode::Call` is not a SQL expression, and erasing it used to
/// emit a `/* unresolved call */` template — a *valid* SQL expression that
/// therefore reached the backend as though it meant something. An unresolved
/// call is a compiler bug; a `Result` is how the caller finds out.
pub fn erase_expr(e: CheckedExpr) -> Result<Expr, Error> {
    let (_phase, _ty, node, _origin) = e.into_parts();
    let expr = match node {
        CheckedExprNode::Column { side, name } => Expr::Col(side, name),
        CheckedExprNode::Lit(l) => Expr::Lit(l),
        CheckedExprNode::Template { sql, args } => {
            Expr::Tpl(sql, erase_exprs(args)?)
        }
        CheckedExprNode::AggTemplate { sql, args } => Expr::Agg(sql, erase_exprs(args)?),
        CheckedExprNode::WinTemplate { sql, args, spec } => Expr::Win(
            sql,
            erase_exprs(args)?,
            Box::new(WinSpec {
                partition: erase_exprs(spec.partition)?,
                order: spec
                    .order
                    .into_iter()
                    .map(|(e, asc)| Ok((erase_expr(e)?, asc)))
                    .collect::<Result<Vec<_>, Error>>()?,
                frame: spec.frame,
            }),
        ),
        CheckedExprNode::In {
            value,
            list,
            negated,
            ..
        } => Expr::In(Box::new(erase_expr(*value)?), erase_exprs(list)?, negated),
        CheckedExprNode::Group(k) => Expr::Group(Box::new(erase_expr(*k)?)),
        CheckedExprNode::Call { name, .. } => {
            return Err(Error::new(format!(
                "internal error: the call `{name}` reached erasure; every call must be \
                 inlined or reduced before a checked query is erased"
            )))
        }
    };
    Ok(expr)
}

/// Erase a list of checked expressions, stopping at the first that cannot be.
fn erase_exprs(es: Vec<CheckedExpr>) -> Result<Vec<Expr>, Error> {
    es.into_iter().map(erase_expr).collect()
}

/// Erase, then run `schema::schema` over the result. The constructors have
/// already proved what this validator re-derives, so it is a debug check.
pub fn erase_checked(query: CheckedQuery) -> Result<(Rel, Vec<String>), String> {
    let rel = erase(query).map_err(|e| e.message)?;
    let cols = crate::schema::schema(&rel)?;
    Ok((rel, cols))
}


/// Strip every `Rel::At` wrapper, so two trees can be compared on structure.
///
/// `At` records *where a stage was written*. The evaluator stamps it at its
/// application sites, and the checked layer stamps it from each node's own
/// origin, so two trees for the same program legitimately carry different
/// locations for the same node — the location is metadata about the source,
/// not part of the relational value. Comparing with `At` in place would
/// compare the producer's choice of origin rather than the trees.
///
/// This is what `root_queries_checked` compares with, which is why it is public.
pub fn without_at(r: &Rel) -> Rel {
    match r {
        Rel::At(_, inner) => without_at(inner),
        Rel::Table {
            schema,
            name,
            columns,
        } => Rel::Table {
            schema: schema.clone(),
            name: name.clone(),
            columns: columns.clone(),
        },
        Rel::Where(i, e) => Rel::Where(Box::new(without_at(i)), e.clone()),
        Rel::Select(i, fs) => Rel::Select(
            Box::new(without_at(i)),
            fs.iter().map(|(n, e)| (n.clone(), e.clone())).collect(),
        ),
        Rel::Update(i, fs) => Rel::Update(
            Box::new(without_at(i)),
            fs.iter().map(|(n, e)| (n.clone(), e.clone())).collect(),
        ),
        Rel::Omit(i, k) => Rel::Omit(Box::new(without_at(i)), k.clone()),
        Rel::Prefix(i, a) => Rel::Prefix(Box::new(without_at(i)), a.clone()),
        Rel::Suffix(i, a) => Rel::Suffix(Box::new(without_at(i)), a.clone()),
        Rel::Agg(i, fs) => Rel::Agg(
            Box::new(without_at(i)),
            fs.iter().map(|(n, e)| (n.clone(), e.clone())).collect(),
        ),
        Rel::Order(i, ks) => Rel::Order(
            Box::new(without_at(i)),
            ks.iter().map(|(e, a)| (e.clone(), *a)).collect(),
        ),
        Rel::Limit(i, n) => Rel::Limit(Box::new(without_at(i)), *n),
        Rel::Offset(i, n) => Rel::Offset(Box::new(without_at(i)), *n),
        Rel::Distinct(i) => Rel::Distinct(Box::new(without_at(i))),
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => Rel::Join {
            kind: *kind,
            left: Box::new(without_at(left)),
            right: Box::new(without_at(right)),
            on: on.clone(),
        },
        Rel::Set { kind, left, right } => Rel::Set {
            kind: *kind,
            left: Box::new(without_at(left)),
            right: Box::new(without_at(right)),
        },
    }
}
// ── reading a `Rel` back into the checked layer ────────────────────────────

/// Which projection a `Rel::Select`-shaped node was.
///
/// `Rel::Select` and `Rel::Agg` are the same Rust variant, so a `Rel` alone
/// does not say which stage built it. A caller migrating from the evaluator
/// knows, and supplies it here rather than receiving a plausible guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageHint {
    Select,
    Agg,
}

/// Rebuild a [`CheckedQuery`] from a `Rel`, its already-computed row, and the
/// one thing a `Rel` cannot say.
///
/// **This is an unchecked bridge, and the name says so.** It is the one
/// function here that produces a `CheckedQuery` *without* running the
/// constructors that establish the layer's guarantees. The previous name
/// (`from_rel`) read like the supported way in; it exists for tests that drive
/// erasure from a hand-built `Rel`, and for a future migration step that holds
/// a `Rel` and knows what it means. Everything else goes through a constructor.
///
/// **Which shapes round-trip.** A `Rel` records each node's own shape but not
/// its input's row and not its expressions' scalar types, so this bridge is
/// *lossy* and is `Err` wherever the loss matters:
///
/// | `Rel` | reconstructible | why not, when it is not |
/// |---|---|---|
/// | `Table { columns: Some(..) }` | yes, with the caller's `row` | — |
/// | `Where`, `Order`, `Limit`, `Offset`, `Distinct`, `Join`, `Set` | yes | the caller supplies each input's row |
/// | `Select` / `Agg` | only with a [`StageHint`] | the two are one variant |
/// | `Prefix` / `Suffix` | only if the affix is invertible on every name | a non-invertible affix loses the input names |
///
/// Expressions are translated structurally (`untyped`) and their scalar types
/// are `Unknown`, because a `Rel` does not carry types. The tree is otherwise
/// *not* re-checked: [`erase`] is the total direction, and this cannot be,
/// because a `CheckedQuery` carries strictly more information than a `Rel`.
/// That asymmetry is the point — see the doc on [`erase`].
pub fn from_rel_unchecked(
    rel: Rel,
    row: RowType,
    hint: Option<StageHint>,
    fallback: Origin,
) -> Result<CheckedQuery, Error> {
    let origin_of = |r: &Rel| match r {
        Rel::At(l, _) => Origin::new(l.module, l.span),
        _ => fallback,
    };
    Ok(match rel {
        Rel::At(_, inner) => return from_rel_unchecked(*inner, row, hint, fallback),
        Rel::Table {
            schema,
            name,
            columns,
        } => {
            // The caller's typed row wins; `columns` only supplies the names.
            // Turning a known `int` into `Unknown` here would discard the one
            // thing the checker worked out.
            let row = if row.columns.is_empty() {
                match columns {
                    Some(cs) => RowType::unknown(cs),
                    None => row,
                }
            } else {
                row
            };
            CheckedQuery {
                row: row.clone(),
                node: CheckedQueryNode::Table {
                    schema,
                    name,
                    columns: row.names(),
                },
                origin: fallback,
            }
        }
        Rel::Where(input, e) => {
            let origin = origin_of(&input);
            // `where` does not change the row, so the output row is the
            // input's; pass it down rather than an empty one.
            let r = row.clone();
            CheckedQuery {
                row,
                node: CheckedQueryNode::Where {
                    input: Box::new(from_rel_unchecked(*input, r, hint, fallback)?),
                    pred: untyped(e),
                },
                origin,
            }
        }
        Rel::Select(input, fs) | Rel::Agg(input, fs) => {
            let hint = hint.ok_or_else(|| {
                Error::new(
                    "a `Rel` projection does not say whether it was a `select` or an `agg`; \
                     pass a `StageHint`",
                )
                .at(fallback)
            })?;
            let origin = origin_of(&input);
            // The stage's *output* row is the caller's `row`; its input's row
            // is not recorded anywhere, so the input is reconstructed with an
            // empty row and its own fields carry the names the caller needs.
            // This is the one place the bridge is knowingly partial.
            let inner = RowType::unknown(vec![]);
            let fields = fs.into_iter().map(|(n, e)| (n, untyped(e))).collect();
            let input = Box::new(from_rel_unchecked(*input, inner, hint_forward(hint), fallback)?);
            let node = match hint {
                StageHint::Select => CheckedQueryNode::Select { input, fields },
                StageHint::Agg => CheckedQueryNode::Agg { input, fields },
            };
            CheckedQuery { row, node, origin }
        }
        Rel::Update(input, fs) => {
            let origin = origin_of(&input);
            let inner = RowType::unknown(vec![]);
            CheckedQuery {
                row,
                node: CheckedQueryNode::Update {
                    input: Box::new(from_rel_unchecked(*input, inner, hint, fallback)?),
                    fields: fs.into_iter().map(|(n, e)| (n, untyped(e))).collect(),
                },
                origin,
            }
        }
        Rel::Omit(input, key) => {
            let origin = origin_of(&input);
            // `omit` removes exactly one column, so the input's row is the
            // output's plus `key` — but only its *names* are recoverable. The
            // *position* the omitted key occupied is not recorded in a `Rel`,
            // and appending it manufactures an input row with its columns
            // shuffled. That is not a theoretical concern: erasing the
            // reconstructed tree then yields a table whose column order differs
            // from the query's, which the differential harness caught with
            // `u : { id, n, a }` and `q = u & omit "n"` (input came back as
            // `id, a, n`).
            //
            // So this reports that the bridge cannot answer, like the other
            // under-determined shapes, rather than guessing. A caller that
            // knows the input row can still reach the node through
            // `CheckedQuery::omit`.
            if !row.has(&key) {
                return Err(Error::new(format!(
                    "cannot recover the input row of `omit \"{key}\"` from a `Rel`: the \
                     omitted column's position is not recorded, and appending it would \
                     reorder the input's columns"
                ))
                .at(origin));
            }
            let mut inner = row.clone();
            if !inner.has(&key) {
                inner
                    .columns
                    .push((key.clone(), crate::core::ScalarType::Unknown));
            }
            CheckedQuery {
                row,
                node: CheckedQueryNode::Omit {
                    input: Box::new(from_rel_unchecked(*input, inner, hint, fallback)?),
                    key,
                },
                origin,
            }
        }
        Rel::Prefix(input, affix) => rename_from(*input, affix, true, row, hint, fallback)?,
        Rel::Suffix(input, affix) => rename_from(*input, affix, false, row, hint, fallback)?,
        Rel::Order(input, keys) => {
            let origin = origin_of(&input);
            let r = row.clone();
            CheckedQuery {
                row,
                node: CheckedQueryNode::Order {
                    input: Box::new(from_rel_unchecked(*input, r, hint, fallback)?),
                    keys: keys.into_iter().map(|(e, asc)| (untyped(e), asc)).collect(),
                },
                origin,
            }
        }
        Rel::Limit(input, n) => {
            let origin = origin_of(&input);
            let r = row.clone();
            CheckedQuery {
                row,
                node: CheckedQueryNode::Limit {
                    input: Box::new(from_rel_unchecked(*input, r, hint, fallback)?),
                    n,
                },
                origin,
            }
        }
        Rel::Offset(input, n) => {
            let origin = origin_of(&input);
            let r = row.clone();
            CheckedQuery {
                row,
                node: CheckedQueryNode::Offset {
                    input: Box::new(from_rel_unchecked(*input, r, hint, fallback)?),
                    n,
                },
                origin,
            }
        }
        Rel::Distinct(input) => {
            let origin = origin_of(&input);
            let r = row.clone();
            CheckedQuery {
                row,
                node: CheckedQueryNode::Distinct(Box::new(from_rel_unchecked(*input, r, hint, fallback)?)),
                origin,
            }
        }
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => {
            let origin = origin_of(&left);
            // A join's two input rows cannot be recovered from its output row
            // alone (the output is the merge of both, with the outer side made
            // nullable). A caller that knows them uses `CheckedQuery::join`.
            let (l, r) = (RowType::unknown(vec![]), RowType::unknown(vec![]));
            CheckedQuery {
                row,
                node: CheckedQueryNode::Join {
                    kind,
                    left: Box::new(from_rel_unchecked(*left, l, hint, fallback)?),
                    right: Box::new(from_rel_unchecked(*right, r, hint, fallback)?),
                    on: untyped(on),
                },
                origin,
            }
        }
        Rel::Set { kind, left, right } => {
            let origin = origin_of(&left);
            // A set operation requires both inputs to expose the output row.
            let (l, r) = (row.clone(), row.clone());
            CheckedQuery {
                row,
                node: CheckedQueryNode::Set {
                    kind,
                    left: Box::new(from_rel_unchecked(*left, l, hint, fallback)?),
                    right: Box::new(from_rel_unchecked(*right, r, hint, fallback)?),
                },
                origin,
            }
        }
    })
}

/// The hint for a projection's *input*. It is the same hint: a `select`'s
/// input is not itself a projection of a different kind, and the caller that
/// supplies one knows the node it is reconstructing.
fn hint_forward(hint: StageHint) -> Option<StageHint> {
    Some(hint)
}

/// Recover the input row of a `prefix`/`suffix` by reversing the rename.
///
/// This is the second place the bridge is knowingly partial: a name that does
/// not carry the affix makes the rename non-invertible, so the input row
/// cannot be recovered. That is an `Err`, not a guess — returning the output
/// row here would claim a rename of names that were never renamed.
fn rename_from(
    input: Rel,
    affix: String,
    prefix: bool,
    row: RowType,
    hint: Option<StageHint>,
    fallback: Origin,
) -> Result<CheckedQuery, Error> {
    let origin = match &input {
        Rel::At(l, _) => Origin::new(l.module, l.span),
        _ => fallback,
    };
    let mut names = Vec::with_capacity(row.columns.len());
    for (n, t) in row.columns() {
        let stripped = if prefix {
            n.strip_prefix(affix.as_str())
        } else {
            n.strip_suffix(affix.as_str())
        };
        match stripped {
            Some(s) => names.push((s.to_string(), t.clone())),
            None => {
                let dir = if prefix { "prefix" } else { "suffix" };
                return Err(Error::new(format!(
                    "column `{n}` does not carry the `{dir} \"{affix}\"` affix, so the input row \
                     of this rename cannot be recovered"
                ))
                .at(origin));
            }
        }
    }
    Ok(CheckedQuery {
        row,
        node: CheckedQueryNode::Rename {
            input: Box::new(from_rel_unchecked(input, RowType::new(names), hint, fallback)?),
            affix,
            prefix,
        },
        origin,
    })
}

// ── structural translation of an untyped expression ───────────────────────

/// An expression from the current IR, as a checked expression.
///
/// The pieces are not re-checked — an expression that reached the IR has
/// already been checked — so this is a structural translation, used by
/// [`from_rel`]. Its scalar type is `Unknown` because a `Rel` does not carry
/// one.
pub fn untyped(e: Expr) -> CheckedExpr {
    let phase = e.phase().unwrap_or(Phase::Row);
    CheckedExpr {
        phase,
        ty: ScalarType::Unknown,
        node: untyped_node(e),
        origin: Origin::new(0, ast::Span::default()),
    }
}

fn untyped_node(e: Expr) -> CheckedExprNode {
    match e {
        Expr::Col(side, name) => CheckedExprNode::Column { side, name },
        Expr::Lit(l) => CheckedExprNode::Lit(l),
        Expr::Tpl(sql, args) => CheckedExprNode::Template {
            sql,
            args: args.into_iter().map(untyped).collect(),
        },
        Expr::In(value, list, negated) => CheckedExprNode::In {
            value: Box::new(untyped(*value)),
            list: list.into_iter().map(untyped).collect(),
            negated,
            elem: ScalarType::Unknown,
        },
        Expr::Agg(sql, args) => CheckedExprNode::AggTemplate {
            sql,
            args: args.into_iter().map(untyped).collect(),
        },
        Expr::Group(k) => CheckedExprNode::Group(Box::new(untyped(*k))),
        Expr::Win(sql, args, spec) => CheckedExprNode::WinTemplate {
            sql,
            args: args.into_iter().map(untyped).collect(),
            spec: WinSpecChecked::from(*spec),
        },
    }
}

impl From<WinSpec> for WinSpecChecked {
    /// A checked window spec from the untyped one, by translating each
    /// expression structurally.
    fn from(spec: WinSpec) -> Self {
        WinSpecChecked {
            partition: spec.partition.into_iter().map(untyped).collect(),
            order: spec
                .order
                .into_iter()
                .map(|(e, asc)| (untyped(e), asc))
                .collect(),
            frame: spec.frame,
        }
    }
}

#[cfg(test)]
mod tests;