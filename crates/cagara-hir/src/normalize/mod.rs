//! The compile-time normalizer: one reduction rule for the whole language.
//!
//! # What this replaces
//!
//! Query construction used to be a *source-directed lowering pass*: a set of
//! entry points, one per family of query shape, each of which walked the syntax
//! in its own order and decided what to build. It carried a second, partial
//! functional normalizer beside it — environments, closures, composition,
//! application, substitution, overload selection, alias expansion and depth
//! budgets — because a pure functional language does not compile by matching
//! the surface spelling of its values.
//!
//! This module is the one normalizer. It reduces the **typed core** — the syntax
//! plus the facts the checker recorded for each node — and it lets the
//! *operation* at the head of an application decide what an argument does,
//! rather than the shape of the enclosing expression. A query primitive that
//! reaches saturation constructs its relational node on the spot ([`arena`]);
//! a `sql` template that reaches saturation constructs its checked expression.
//! Nothing here knows which stages exist, which operators are pipelines, or
//! which spellings are aliases: those are facts on the definitions, read from
//! the checker through `Resolved`.
//!
//! # The reduction rules
//!
//! ```text
//! normalize  (Name/Proj)   = a local, or the body of the definition it resolved to
//! normalize  (Field/Lit)   = a checked column or literal
//! normalize  (Lambda p… e) = a closure over the environment
//! normalize  (Case)        = a searched CASE template
//! normalize  (Record/List) = a record or list of the arguments' values
//! normalize  (Sql s)       = a template awaiting its arguments
//! normalize  (App h a…)    = foldl apply (normalize h) (map normalize a…)
//!
//! apply (Closure, v)        = bind the next parameter; evaluate the body when
//!                             it was the last one
//! apply (Template, v)       = accumulate; build the checked expression when
//!                             every argument has arrived
//! apply (Direction, v)      = a sort key
//! apply (Group, v)          = a grouping key
//! apply (Membership, v)     = accumulate; build `in` when saturated
//! apply (Stage s, v)        = accumulate; build the relational node when
//!                             saturated
//! apply (Join/Set c, v)     = accumulate; build the join/set node when
//!                             saturated
//! apply (Table/Label, v)    = a column, a name, or a count
//! apply (Overloads cands, v)= the first candidate that applies
//! ```
//!
//! Two properties of this definition are load-bearing:
//!
//! * **Call by name, never memoized.** An argument is evaluated at the use site
//!   it was written at, so it keeps that site's typed node — its type, row,
//!   origin and overload choices. Evaluation is not shared between two uses of
//!   one definition, so a helper used twice produces two stage chains with two
//!   sites, which is what the checker recorded for each of them.
//! * **The environment is the substitution.** It is captured per application
//!   rather than substituted into syntax, so an argument can never be captured
//!   by a binder it was passed under. The term oracle keeps its own textual,
//!   capture-avoiding substituter on purpose: two independent reductions are
//!   what let the oracle disagree with production, and disagreement is the
//!   property the agreement tests exist to check.
//!
//! # Budgets
//!
//! One [`budget::Budget`] replaces the depth constants five separate passes
//! carried: a recursion bound (how many definitions down one path) and a work
//! bound (nodes evaluated and nodes built). Exceeding either is
//! [`Control::Unsupported`] — the accepted-program boundary that becomes a
//! compiler fault, never a crash and never a wrong query.

pub(crate) mod arena;
pub(crate) mod budget;
mod env;
mod frame;
mod route;
mod scalar;
mod thunk;
mod value;

use crate::compile::CompilerInput;
use crate::core::{Error, Origin};
use cagara_syntax::ast::{Expr, Span};

pub(crate) use env::Env;

/// What a reduction can stop on.
///
/// The old lowering returned `Option` from each entry point, tried the next one
/// when an attempt declined, and carried a private error type for constructor
/// disagreements: several dialects for one question. This is that question,
/// asked once:
///
/// * [`Control::Unsupported`] — the normalizer has no rule for an accepted
///   construct. In a total pipeline this is a compiler capability gap.
/// * [`Control::Program`] — the checker accepted the program but building it
///   says the program is wrong (a table whose columns are unknown, a `limit`
///   with a negative count). That diagnostic is the program's to fix.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Control {
    Unsupported { origin: Origin, what: String },
    Program(Error),
}

impl Control {
    /// A capability gap at `origin`.
    pub(crate) fn unsupported(origin: Origin, what: impl Into<String>) -> Self {
        Control::Unsupported {
            origin,
            what: what.into(),
        }
    }

    /// The error the pipeline reports, classified by who is at fault.
    pub(crate) fn into_error(self) -> Error {
        match self {
            Control::Unsupported { origin, what } => Error::unsupported(what).at(origin),
            Control::Program(error) => error,
        }
    }
}

impl From<Error> for Control {
    fn from(error: Error) -> Self {
        Control::Program(error)
    }
}

/// A body being evaluated: the definition it belongs to and its syntax.
///
/// Carried as a unit because evaluation crosses module boundaries whenever it
/// follows a definition — a user query reaches a prelude helper and that
/// helper's body is recorded under the *prelude's* module index, so the module
/// is not ambient and cannot be the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BodyRef<'a> {
    module: usize,
    def: usize,
    expr: &'a Expr,
}

/// A value the normalizer reduces to.
///
/// Values come in two shapes. A *function value* is either a closure —— a
/// lambda and the environment it closed over — or one of the accumulating
/// forms: a stage, a join or set, a template. An accumulated form owns its
/// arguments so far, in the declaration order of the operation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value<'a> {
    /// The name a definition could not be resolved to.
    ///
    /// A primitive body is never reached through a *use*, so the parser leaves
    /// the spelling's id zero and the checker records no resolution for it.
    /// The definition that spells the primitive is still the answer: `_&_ : a
    /// -> (a -> b) -> b = x => f => f x` is a lambda, and applying it is what
    /// replaces it. This keeps the name so the operation can be found.
    Name(String),

    /// A lambda and the environment it closed over.
    Closure(Closure<'a>),
    /// A built relational node in the query being constructed.
    Query(arena::NodeId),
    /// A checked scalar expression.
    Scalar(crate::checked::CheckedExpr),
    /// A query operation awaiting its input. What a partially applied stage
    /// denotes.
    Stage(StageFn),
    /// A join or set operation awaiting its remaining operand.
    Combine(CombineFn),
    /// A `sql` template awaiting its arguments.
    Template(TemplateFn),
    /// `table "s" "t"`, awaiting its two name arguments.
    Table {
        args: Vec<Value<'static>>,
        /// The row the checker closed for the call.
        row: Option<Vec<(String, crate::core::ScalarType)>>,
        origin: Origin,
    },
    /// A `symbol` argument: an affix, a removed column, a value wrapper, or a
    /// count.
    Label(Label),
    /// `asc expr` / `desc expr`, awaiting the key.
    Direction {
        ascending: bool,
        args: Vec<Value<'static>>,
        origin: Origin,
    },
    /// A sort key: the expression and its direction.
    SortKey(crate::checked::CheckedExpr, bool),
    /// `group expr`, awaiting the key.
    Group {
        arg: Option<crate::checked::CheckedExpr>,
        origin: Origin,
    },
    /// `in [..] v`, collecting the list, then the value.
    Membership {
        list: Option<Vec<crate::checked::CheckedExpr>>,
        value: Option<crate::checked::CheckedExpr>,
        origin: Origin,
    },
    /// A complete window specification, consumed by a window template.
    WinSpec(Box<crate::checked::WinSpecChecked>),
    /// A row frame, consumed by a window specification.
    Frame(crate::ir::Frame),
    /// A window frame bound, consumed by a row frame.
    Bound(crate::ir::Bound),
    /// A frame accumulator: `rows a b`, `preceding n`, `following n`.
    Fold {
        partial: Vec<Value<'static>>,
        origin: Origin,
        spec: frame::FoldSpec,
    },
    /// The candidates of an overloaded name, when the use site itself recorded
    /// no choice.
    Overloads(Vec<Value<'a>>),
    /// A record or a list: its values in source order, and — for a record — its
    /// field names, because a projection's output row is stated by the checker
    /// in record order and the two must not drift.
    Tuple {
        items: Vec<Value<'a>>,
        names: Option<Vec<String>>,
    },
}

#[allow(dead_code)] // The erase boundary reads these once the route is cut over.
impl<'a> Value<'a> {
    /// Whether this value is a query already built in the arena.
    fn as_query(&self) -> Option<arena::NodeId> {
        match self {
            Value::Query(id) => Some(*id),
            _ => None,
        }
    }

    /// The bare function this value denotes, if it is the identity function.
    ///
    /// One syntactic clause: a lambda whose body is its own parameter.
    /// `id : a -> a = x => x` is applied away by β-reduction wherever it is
    /// *applied*, but a composition written `>>> id` never applies it — the
    /// composition itself is a value — so the reduction has to recognise it
    /// as a value that composes to nothing, which is exactly what the identity
    /// function is. The term oracle erases the same definition by reducing its
    /// body, so both routes agree.
    pub(crate) fn is_identity(&self) -> bool {
        let Value::Closure(closure) = self else {
            return false;
        };
        if closure.params.len() != 1 {
            return false;
        }
        let parameter = &closure.params[0];
        matches!(&closure.body.expr.kind, cagara_syntax::ast::ExprKind::Name(name) if name == parameter)
            && closure.env.is_empty()
    }
}

/// A symbolic label: a stage argument that names something rather than
/// computing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Label {
    pub(crate) text: String,
    /// Set when the label came from an integer literal, so a count stage reads
    /// it without parsing a spelling back.
    pub(crate) int: Option<i64>,
}

impl Label {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Label {
            text: text.into(),
            int: None,
        }
    }

    pub(crate) fn int(value: i64) -> Self {
        Label {
            text: value.to_string(),
            int: Some(value),
        }
    }
}

/// A function value: parameters still to bind, the body, and the environment.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Closure<'a> {
    params: std::sync::Arc<Vec<String>>,
    /// How many parameters have been bound.
    bound: usize,
    body: BodyRef<'a>,
    env: std::sync::Arc<Env<'a>>,
    /// The application this closure is being applied at, once it has one.
    ///
    /// A lambda body is evaluated *later* — when its last argument arrives —
    /// and the reduction of the body must still attribute what it builds to the
    /// application that supplied the arguments. The checker records a pipeline
    /// application's stage against the definition that wrote `&`, so without
    /// this the body's frames name the body and the record is missed.
    site: Option<Site>,
}

/// A query operation, and the arguments it has so far, in declaration order.
///
/// The normalizer never asks which stage this is beyond applying its arguments,
/// so a stage added to the prelude needs no change here.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StageFn {
    spec: StageSpec,
    args: Vec<Value<'static>>,
}

/// The identity and arity of a stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StageSpec {
    kind: arena::QueryNodeKind,
    arity: usize,
}

/// A join or set operation awaiting an operand.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CombineFn {
    Join {
        kind: crate::ir::JoinKind,
        args: Vec<Value<'static>>,
    },
    Set {
        kind: crate::ir::SetKind,
        args: Vec<Value<'static>>,
    },
}

impl CombineFn {
    fn arity(&self) -> usize {
        match self {
            CombineFn::Join { .. } => 3,
            CombineFn::Set { .. } => 2,
        }
    }
}

/// A template awaiting its arguments.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TemplateFn {
    sql: String,
    kind: crate::check::TemplateKind,
    /// How many expression arguments it takes.
    arity: usize,
    /// A window template's specification, once it has arrived.
    spec: Option<Box<crate::checked::WinSpecChecked>>,
    ty: crate::core::ScalarType,
    args: Vec<crate::checked::CheckedExpr>,
    origin: Origin,
}

/// The state of one definition's evaluation.
///
/// Owned per attempt and threaded through the reduction, so two compilations
/// never share lowering state — the property a thread-local active set made
/// impossible.
pub(crate) struct Normalizer<'i, 'a> {
    /// The input lifetime and the value lifetime, so a value's syntax is tied to
    /// the workspace it came from.
    pub(crate) _marker: std::marker::PhantomData<&'a ()>,
    pub(crate) input: CompilerInput<'i>,
    /// The arena every node is constructed into.
    pub(crate) builder: arena::Builder,
    pub(crate) budget: budget::Budget,
    /// The definitions on the current evaluation path.
    pub(crate) active: Vec<(usize, usize)>,
    /// Which definition's body is being evaluated, so a node's site can name
    /// the definition it was written in.
    pub(crate) defining: usize,
    /// The module the syntax under evaluation was written in.
    pub(crate) module: usize,
    /// Where the expression being evaluated is: the origin a construction
    /// The syntax being evaluated, outermost first: every node of the typed
    /// core the reduction is currently inside. See [`Site`].
    pub(crate) sites: Vec<Site>,
}

impl<'i, 'a> Normalizer<'i, 'a> {
    fn new(input: CompilerInput<'i>) -> Self {
        Normalizer {
            input,
            builder: arena::Builder::new(),
            budget: budget::Budget::new(),
            active: Vec::new(),
            defining: 0,
            module: 0,
            sites: Vec::new(),
            _marker: std::marker::PhantomData,
        }
    }

    /// The checker's typed facts for one node.
    ///
    /// The typed tree is keyed by `ExprId`; a definition that failed has none,
    /// which cannot happen here because evaluation runs only for definitions
    /// the checker accepted.
    pub(crate) fn typed(&self, module: usize, expr: &Expr) -> Option<crate::check::TypedExpr> {
        self.facts(module, expr)
    }

    /// Where evaluation currently is.
    pub(crate) fn origin(&self) -> Origin {
        self.sites.last().map_or_else(
            || Origin::new(self.module, Span::default()),
            |site| site.origin(),
        )
    }

    /// The innermost frame that carries its own identity.
    pub(crate) fn call_site(&self) -> Option<&Site> {
        self.sites.iter().rev().find(|site| site.named)
    }

    /// The module and its syntax an argument of the application being
    /// reduced was written in.
    pub(crate) fn call_expr(&self) -> Result<&Expr, Control> {
        let site = self
            .sites
            .last()
            .ok_or_else(|| Control::unsupported(self.origin(), "no expression"))?;
        Ok(&self.input.module(site.module).def(site.def).body)
    }

    /// The module and expression an application argument was written in.
    ///
    /// `in [..] v` is the one operation whose list is not preceded by a
    /// template, so its items are converted at the call site.
    pub(crate) fn list_origin(&self) -> Result<(usize, &Expr), Control> {
        let site = self
            .call_site()
            .ok_or_else(|| Control::unsupported(self.origin(), "no call"))?;
        Ok((
            site.module,
            &self.input.module(site.module).def(site.def).body,
        ))
    }

    /// The module the expression being evaluated was written in.
    pub(crate) fn call_module(&self) -> usize {
        self.sites.last().map_or(self.module, |site| site.module)
    }

    /// The `ExprId` of the expression being evaluated.
    pub(crate) fn call_id(&self) -> u32 {
        self.sites.last().map_or(0, |site| site.id)
    }

    /// The facts the checker recorded for one expression.
    ///
    /// The typed tree is keyed by `(module, def)` and holds a node per
    /// expression of that body, so the question "what did the checker record
    /// here" needs both: the expression's *path* — the chain of ids from the
    /// body's root, because an id alone is ambiguous — and the definitions
    /// whose bodies contain it, innermost first.
    pub(crate) fn facts(&self, module: usize, expr: &Expr) -> Option<crate::check::TypedExpr> {
        self.node_at_path(self.path_to(module, expr))
    }

    /// Every frame from the innermost outwards, as `(module, definition, span)`.
    ///
    /// The span rather than the id: two nodes of one body can share an id — a
    /// stage application and the callee it applies — and the frames are told
    /// apart by where they are written.
    fn frames(&self) -> impl Iterator<Item = (usize, usize, Span)> + '_ {
        self.sites
            .iter()
            .rev()
            .map(|site| (site.module, site.def, site.span))
    }

    /// The definitions on the stack, innermost first, once each.
    fn frame_defs(&self) -> Vec<(usize, usize)> {
        let mut seen = Vec::new();
        for (module, def, _) in self.frames() {
            if !seen.contains(&(module, def)) {
                seen.push((module, def));
            }
        }
        seen
    }

    /// The path to `expr` in `module`: the parents of the innermost frame that
    /// sits at it, then its own id.
    ///
    /// A frame that is `expr` *is* it, so the expression's path is the frame's
    /// parents. A frame that is something else is a descendant — an application
    /// argument, say — and the expression is one of its ancestors, found by
    /// walking the tree from its root.
    ///
    /// A frame's id alone does not say which node it is, which is why a frame
    /// carries its span as well: the checker stamps a stage application and its
    /// callee with the same id, and only the span says which is meant.
    fn path_to(&self, module: usize, expr: &Expr) -> Option<Vec<u32>> {
        let tc = self.input.type_check();
        for site in self.sites.iter().rev() {
            if site.module != module {
                continue;
            }
            let Some(tree) = tc.typed_body(module, site.def) else {
                continue;
            };
            if site.id == expr.id && site.span == expr.span {
                let mut path: Vec<u32> = site.parent.into_iter().collect();
                path.push(site.id);
                return Some(path);
            }
            if let Some(path) = ancestor_path(tree, expr.id) {
                return Some(path);
            }
            if let Some(path) = ancestor_path_at(tree, expr.id, expr.span) {
                return Some(path);
            }
        }
        None
    }

    /// The node at a path, in the innermost definition that has one there.
    ///
    /// The frames are the path from the definition being evaluated to the
    /// syntax, so the innermost frame whose body *contains* this path is the
    /// one that wrote the expression being read. A frame further out holds the
    /// same path — a helper's body is the same syntax whether it is reached
    /// from one use or another — but recorded the types of *its* instantiation,
    /// which is exactly what a helper applied at two different row types must
    /// not be read as.
    ///
    /// The two questions asked of a node have different answers, though, and
    /// are answered separately: the *resolution* is the writing definition's
    /// (a generic helper writes `_+_` with its overload still open, and only
    /// the caller knows which `+` it meant), while the *type* is the
    /// instantiation's, so a concrete type anywhere on the stack wins.
    fn node_at_path(&self, path: Option<Vec<u32>>) -> Option<crate::check::TypedExpr> {
        let path = path?;
        let tc = self.input.type_check();
        let mut found = None;
        let mut typed = None;
        let mut defs = self.frame_defs();
        defs.push((self.module, self.defining));
        for (module, def) in defs {
            let Some(node) = tc
                .typed_body(module, def)
                .and_then(|tree| follow(tree, &path))
            else {
                continue;
            };
            if node
                .ty()
                .is_some_and(|ty| ty != crate::core::ScalarType::Unknown)
            {
                typed.get_or_insert_with(|| node.clone());
            }
            found.get_or_insert_with(|| node.clone());
        }
        Some(typed.or(found)?)
    }
    /// The definition a name denotes in the scope of the definition being
    /// evaluated.
    ///
    /// The checker's resolutions are what the reduction normally reads, and
    /// they are recorded for every name a *use* reaches. A primitive
    /// definition's own body is not such a place — nothing applies
    /// `_&_ : a -> (a -> b) -> b = x => f => f x` from inside it, and its
    /// spelling has no resolution — so this is the fallback: the scope the body
    /// was written in says what the name means.
    ///
    /// The first definition of the name answers. Narrowing overloads is a use
    /// site's job, and this path exists precisely where there is no use site.
    pub(crate) fn scope_definition(&self, module: usize, name: &str) -> Option<(usize, usize)> {
        match self.input.module(module).scope().get(name)? {
            crate::workspace::Binding::Def(target_module, target_def) => {
                Some((*target_module, *target_def))
            }
            crate::workspace::Binding::Overloads(target_module, defs) => {
                Some((*target_module, *defs.first()?))
            }
            crate::workspace::Binding::Module(_) => None,
        }
    }

    /// The resolution the checker recorded at a name occurrence.
    pub(crate) fn resolution(&self, module: usize, expr: &Expr) -> Option<crate::check::Resolved> {
        self.facts(module, expr).and_then(|node| node.resolved)
    }

    /// The checker's scalar type for the expression whose frame is innermost.
    ///
    /// This is the *application* being reduced — the node that applied a
    /// template — not the template's own declared body, which is why the
    /// stack is read rather than a definition.
    pub(crate) fn ty_at(&self, _origin: Origin) -> Option<crate::core::ScalarType> {
        let site = self.sites.last()?;
        let tree = self.input.type_check().typed_body(site.module, site.def)?;
        tree.node_at(site.span).and_then(|node| node.ty())
    }

    /// The row the checker closed for a query-shaped expression.
    pub(crate) fn row(
        &self,
        module: usize,
        expr: &Expr,
    ) -> Option<Vec<(String, crate::core::ScalarType)>> {
        self.typed(module, expr).and_then(|node| node.row())
    }

    /// A definition's declared arity: how many arrows its signature has.
    pub(crate) fn declared_arity(&self, module: usize, def: usize) -> usize {
        let mut count = 0;
        let mut ty = self.input.module(module).def(def).ty.as_ref();
        while let Some(cagara_syntax::ast::TypeExpr::Fun(_, rest)) = ty {
            count += 1;
            ty = Some(rest);
        }
        count
    }

    /// The declared result kind of a definition's `sql` template.
    pub(crate) fn declared_template_kind(
        &self,
        module: usize,
        def: usize,
    ) -> crate::check::TemplateKind {
        crate::check::declared_template_kind(self.input.module(module).def(def))
    }
}

/// One node of the typed core the reduction is inside.
///
/// A stage is *applied* by the definition whose body spells the application —
/// the prelude, for a primitive — and the checker recorded the application
/// there. So the node a reduction builds is identified by the innermost
/// application on the stack, and blamed where that application was written.
/// The frames in between matter too: a lambda is not an application, so an
/// application inside a lambda body belongs to the frame below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) module: usize,
    pub(crate) def: usize,
    pub(crate) id: u32,
    pub(crate) span: Span,
    /// Whether this frame carries its own identity.
    ///
    /// An expression that is another expression's *argument* does not: it is
    /// part of the enclosing application, and a record the checker made for it
    /// — the stage a lambda body applies — must not be mistaken for the
    /// application's. Everything else (a definition body, an applied query
    /// definition) does.
    pub(crate) named: bool,
    /// The expression this frame is part of: the application it is an argument
    /// of, or the body it is a definition of.
    ///
    /// `ExprId`s are unique only within the *parse* of one module — the prelude
    /// is parsed once and its `_&_` body carries the same id every time the
    /// prelude is imported — so a node is identified by its whole path, not by
    /// its id. The frame below names the parent, which is what makes the path.
    pub(crate) parent: Option<u32>,
}

impl Site {
    pub(crate) fn origin(&self) -> Origin {
        Origin::new(self.module, self.span)
    }
}

/// What evaluating one definition produced.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lowered {
    pub(crate) query: arena::CompiledQuery,
}

impl Lowered {
    pub(crate) fn into_query(self) -> arena::CompiledQuery {
        self.query
    }
}

/// Evaluate one definition's body to a query.
///
/// A query definition's scheme says its body is a query, so a body that reduces
/// to anything else is a capability gap: the checker said this is a query and
/// the normalizer could not build one.
pub(crate) fn definition(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<Lowered, Control> {
    let mut normalizer = Normalizer::new(input);
    normalizer.defining = def;
    normalizer.module = module;
    let origin = Origin::new(module, body.span);
    normalizer.budget.enter(origin)?;
    normalizer.active.push((module, def));
    let value = normalizer.body_value(module, def, body);
    normalizer.active.pop();
    let Value::Query(root) = value? else {
        return Err(Control::unsupported(
            origin,
            "a query definition evaluated to something that is not a query",
        ));
    };
    let builder = std::mem::replace(&mut normalizer.builder, arena::Builder::new());
    Ok(Lowered {
        query: builder.finish(root),
    })
}

/// One definition's body, evaluated and erased, for the differential test.
///
/// The two routes are compared through this and `legacy_debug_for_tests`; it is
/// not an interface to build on.
#[doc(hidden)]
pub fn normalize_debug_for_tests(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    body: &Expr,
) -> Result<crate::ir::Rel, String> {
    definition(input, module, def, body)
        .map_err(|control| format!("{control:?}"))
        .and_then(|lowered| lowered.into_query().erase().map_err(|error| error.message))
}

/// The typed node at `path` in `tree`, when the body has one there.
///
/// The path is the chain of `ExprId`s from the body's own expression down, and
/// it is what identifies a node: an id alone does not, because one parse of the
/// prelude serves every module that imports it.
fn follow<'a>(
    tree: &'a crate::check::TypedBody,
    path: &[u32],
) -> Option<&'a crate::check::TypedExpr> {
    let (root, rest) = path.split_first()?;
    let root_node = tree.root();
    if root_node.id != *root {
        return None;
    }
    rest.iter().try_fold(root_node, |node, id| {
        // A body can hold two nodes with one id — the checker stamps a stage
        // application and the callee it applies alike — so the path names the
        // *occurrence*: the resolution that built the path keeps the first
        // child with that id, and so does this. The ambiguity is harmless
        // because the two nodes describe one expression.
        node.children
            .iter()
            .filter_map(|at| tree.nodes().get(*at))
            .find(|child| child.id == *id)
    })
}

/// The path to the node with `id` inside one typed body: its ancestors' ids and
/// then its own.
fn ancestor_path(tree: &crate::check::TypedBody, id: u32) -> Option<Vec<u32>> {
    fn walk(
        tree: &crate::check::TypedBody,
        node: &crate::check::TypedExpr,
        id: u32,
        path: &mut Vec<u32>,
    ) -> bool {
        path.push(node.id);
        if node.id == id {
            return true;
        }
        for child in tree.children(node) {
            if walk(tree, child, id, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    walk(tree, tree.root(), id, &mut path).then_some(path)
}

/// The path of the node at `id` *and* `span`, when an id alone is ambiguous.
fn ancestor_path_at(tree: &crate::check::TypedBody, id: u32, span: Span) -> Option<Vec<u32>> {
    fn walk_at(
        tree: &crate::check::TypedBody,
        node: &crate::check::TypedExpr,
        id: u32,
        span: Span,
        path: &mut Vec<u32>,
    ) -> bool {
        path.push(node.id);
        if node.id == id && node.span == span {
            return true;
        }
        for child in tree.children(node) {
            if walk_at(tree, child, id, span, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    walk_at(tree, tree.root(), id, span, &mut path).then_some(path)
}
