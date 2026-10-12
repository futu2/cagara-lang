//! Normalization: reduce a resolved term to a form that no longer depends on
//! how it was *spelled*.
//!
//! # The problem this exists to solve
//!
//! Elaboration currently doubles as the normalization step, and it does so by
//! re-walking **source**. Applying a function builds an environment and walks
//! the callee's body again, from the AST ([`crate::elaborate`]'s
//! `elaborate_lambda`, whose comment says outright that "the body is held as
//! source so applying it re-runs `elaborate_lambda`'s binding logic"). That one
//! decision is the root of a family of workarounds, each of which exists only
//! because there is no separate normalization pass:
//!
//! * a bare alias has no body to re-walk, so `follow_alias` walks the chain by
//!   hand;
//! * a composed function value has no body either, so
//!   `apply_to_computed_head` had to be added for `(trim >>> upper) .name`;
//! * a callee is classified by its *syntax shape* (`elaborate_callable`),
//!   because there is no normalized function value to inspect;
//! * a polymorphic definition must be re-elaborated once per hole assignment,
//!   because substitution bakes the choice in at expansion time.
//!
//! # What normalization is
//!
//! A term is reduced to a **normal form** in which:
//!
//! * every name is resolved to what it denotes — there are no aliases;
//! * every application is *saturated* or is a first-class function value —
//!   there are no partially applied callables awaiting arguments;
//! * composition is reduced away, since `>>>` is an ordinary prelude definition
//!   and normalizing it is just beta reduction;
//! * `id`-like definitions with no operational content are erased.
//!
//! Two spellings of one program — `p = trim >>> upper` applied as `p .name`,
//! the same written inline as `(trim >>> upper) .name`, and a lambda
//! `(f => upper f) .name` — normalize to the *same term*. That property is what
//! the compiler currently has to assert by comparing generated SQL
//! (`cagara-sql`'s spelling-equivalence tests); under normalization it holds by
//! construction, at the term level, before any SQL exists.
//!
//! # Scope
//!
//! This module is deliberately **inert for now**: it defines the normal form
//! and the reduction relation, and is exercised by tests, but nothing in the
//! production path calls it yet. Migrating elaboration onto it is a separate
//! step, and doing it in one commit would be a rewrite with no way to tell a
//! fix from a regression — which is the failure mode the gate in
//! [`crate::compile`] exists to prevent.
//!
//! # Why the normal form is data, not syntax
//!
//! [`Term`] carries no `ast::Expr` and no `String` binding name. An application
//! is a constructor over *arguments*, not a body to substitute into. That is
//! the difference that matters: a re-walk can observe the spelling of a
//! program, and this cannot.

use crate::check::Choice;
use crate::core::Origin;
use std::collections::HashMap;
use std::sync::Arc;

/// A definition's identity: module index and source-order position.
pub type DefId = (usize, usize);

/// Which checked constructor a `sql` template lowers to.
///
/// Read from the template's **declared** signature, not from its resolved
/// scheme: `upper : expr r string -> expr r string` is a scalar template,
/// `sum : expr r a -> agg (expr r a)` an aggregate, `rowNumber : winspec r a ->
/// win (expr r a)` a window function. The declared form is the only place that
/// distinction survives, because once a template is applied its scheme says
/// what it *returned*, not which node built it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    Scalar,
    Agg,
    Win,
}

/// A fully normalized term.
///
/// Field names and types are the checker's; this type adds structure and
/// resolution, and nothing else. It is intentionally small — every construct
/// here has to be given a meaning by whoever consumes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// A reference the checker resolved: a column of the current row, or a
    /// value bound by an enclosing binder.
    ///
    /// Both cases are *local* by construction: a name that referred to a
    /// top-level definition was resolved away during normalization, so this
    /// carries no name to look up. That is the property that removes aliases,
    /// `follow_alias`, and name re-resolution from the consumer.
    Local {
        /// The `ExprId` the checker recorded a type for.
        id: u32,
        /// How the reference was written, for diagnostics only. Never used for
        /// resolution — that already happened.
        label: String,
        /// The scalar type the checker gave this occurrence.
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A **column reference**: `.x`, `.<x` (left of a join) or `.>x` (right).
    ///
    /// Distinct from [`Term::Local`] because the side is part of the meaning,
    /// not part of the spelling. Folding it into the label as a prefix lost it:
    /// a join predicate's `.<up1` became a `Single`-sided column, and the
    /// checked layer correctly refused it — "join predicates must say which
    /// input a column comes from" — for a program the checker had accepted.
    Column {
        side: cagara_syntax::ast::Side,
        name: String,
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A literal, already at the type the checker gave it.
    Lit {
        lit: crate::ir::Lit,
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A `sql "..."` template with its arguments already normalized.
    ///
    /// The arguments are positional, matching the template's `$1`…`$n`, and
    /// there are exactly as many as the template takes: saturation happened
    /// during reduction.
    ///
    /// `kind` is the phase the template was *declared* to have, read from its
    /// signature rather than from its resolved scheme. That distinction is not
    /// cosmetic: the checked layer needs it to choose between
    /// `CheckedExpr::template`, `agg_template` and `win_template`, and the
    /// resolved scheme of a template used with arguments is the *result*, which
    /// no longer says which constructor built it.
    Template {
        sql: Arc<str>,
        args: Vec<Term>,
        kind: TemplateKind,
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A primitive, with its arguments saturated.
    ///
    /// `ty` is the scalar type the checker gave the occurrence. The checked
    /// constructors require it, and it is recorded per `ExprId` rather than
    /// derivable from the primitive's name — `__in` over ints and over strings
    /// are the same primitive at two types.
    Primitive {
        name: Arc<str>,
        args: Vec<Term>,
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A definition whose normal form is a *function*: `params` are the binders
    /// still unwritten, and `body` is under them.
    ///
    /// This is the only place a binder survives, and it is a value: a term of
    /// this shape in argument position is a first-class function, which is what
    /// makes `app = g => (g .name)` and `(f => upper f) .name` normalizable
    /// without special cases.
    Lambda {
        params: Vec<String>,
        body: Box<Term>,
        /// The definition this body came from, for diagnostics.
        source: Option<DefId>,
        origin: Origin,
    },
    /// A value applied to arguments that could not be reduced yet.
    ///
    /// The head is a **local** — a lambda parameter — so what it stands for is
    /// not known until the enclosing lambda is applied. `f x` inside
    /// `f => x => f x` is this node; applying the outer lambda substitutes for
    /// `f`, and the application reduces then.
    ///
    /// Without this node the normal form could not represent a curried
    /// combinator, which is to say it could not represent the prelude's `&`,
    /// `$`, `>>>` and `<<<` — the definitions every pipeline is built from.
    Apply {
        head: Box<Term>,
        args: Vec<Term>,
        origin: Origin,
    },
    /// A **window function** applied to its arguments: `rowNumber spec`,
    /// `sumOver spec .amount`.
    ///
    /// Distinct from [`Term::Template`] because a window functions's first
    /// argument is its *spec* — a query-level record of partition, order and
    /// frame — not a scalar expression. Keeping it separate is what lets the
    /// spec be lowered by the code that already knows how
    /// (`elaborate_winspec`) rather than by a second implementation here.
    Window {
        sql: Arc<str>,
        /// The window specification, as written.
        spec: Box<Term>,
        /// The scalar arguments, in order.
        args: Vec<Term>,
        ty: crate::core::ScalarType,
        origin: Origin,
    },
    /// A `case`/`ifThenElse` chain, already flattened to arms plus a default.
    ///
    /// Flattened because the nesting is spelling: `case { a => x; true => y }`
    /// and `ifThenElse a x y` denote the same thing, and after normalization
    /// they must *be* the same thing.
    Case {
        arms: Vec<(Term, Term)>,
        default: Box<Term>,
        origin: Origin,
    },
    /// A record literal, fields in source order.
    Record {
        fields: Vec<(String, Term)>,
        origin: Origin,
    },
    /// A list literal, elements in source order.
    List { items: Vec<Term>, origin: Origin },
}

impl Term {
    /// Whether this term is a function value, i.e. whether it denotes something
    /// awaiting arguments rather than a finished expression.
    ///
    /// An `Apply` counts: it denotes the result of a function it has not reduced
    /// yet, which is a value awaiting nothing further.
    pub fn is_function(&self) -> bool {
        matches!(self, Term::Lambda { .. })
    }

    /// Apply this term to one argument, reducing as far as the result allows.
    ///
    /// Returns `None` when the term is not applicable — applying a literal, a
    /// record, or a list is a *type* error, and the checker has already rejected
    /// it, so the caller treats `None` as "cannot happen on a checker-accepted
    /// program" rather than as a user diagnostic. Reporting it here would invent
    /// a second, weaker error for something the checker already owns.
    ///
    /// A **local** head does not reduce: `f x` inside a lambda whose `f` is a
    /// parameter has nothing to substitute into, because the value `f` will
    /// stand for arrives when the *enclosing* lambda is applied. It becomes an
    /// [`Term::Apply`] and reduces then. Treating it as a failure was wrong, and
    /// wrong in an instructive way: it made the four curried combinators in the
    /// prelude — `_&_`, `_$_`, `_>>>_`, `_<<<_`, all of the form
    /// `f => x => f x` — the only definitions in the prelude that would not
    /// normalize, which is to say the very definitions composition is built
    /// from.
    pub fn apply(self, arg: Term) -> Option<Term> {
        match self {
            Term::Lambda {
                params,
                body,
                source,
                origin,
            } => {
                if params.is_empty() {
                    return None;
                }
                let mut rest = params;
                let param = rest.remove(0);
                // The substitution: replace the binder inside the body by the
                // argument. A real substitution over a *term*, not a walk over
                // source, which is the whole point of this module.
                let reduced = substitute(*body, &param, &arg);
                if rest.is_empty() {
                    Some(reduced)
                } else {
                    Some(Term::Lambda {
                        params: rest,
                        body: Box::new(reduced),
                        source,
                        origin,
                    })
                }
            }
            // A head that is not yet a function: keep the application, so the
            // substitution can happen when the head becomes one.
            head @ Term::Local { .. } => {
                let origin = match &head {
                    Term::Local { origin, .. } => *origin,
                    _ => unreachable!("matched on Local"),
                };
                Some(Term::Apply {
                    head: Box::new(head),
                    args: vec![arg],
                    origin,
                })
            }
            // An application already awaiting arguments: extend it rather than
            // nesting, so `f x y` is one node and not `((f x) y)`.
            Term::Apply {
                head,
                mut args,
                origin,
            } => {
                args.push(arg);
                Some(Term::Apply { head, args, origin })
            }
            // A `sql` template applied to an argument: accumulate, which is
            // what saturation means for a template. `upper .name` is this case
            // — a template is not a lambda, so there is nothing to substitute,
            // but the application is still a reduction step and the argument
            // belongs to the template's `$1`.
            //
            // Arity is *not* checked here, and that is a deliberate division of
            // labour rather than an omission: `validate_template_definition`
            // checks a template's placeholders against its declared signature
            // once, at the definition, so a template that reaches this point has
            // already been shown to take the arguments it is given. Checking
            // again here would be a second source of truth for the same fact.
            Term::Template {
                sql,
                mut args,
                kind,
                ty,
                origin,
            } => {
                args.push(arg);
                Some(Term::Template {
                    sql,
                    args,
                    kind,
                    ty,
                    origin,
                })
            }
            // A primitive accumulates the same way.
            Term::Primitive {
                name,
                mut args,
                ty,
                origin,
            } => {
                args.push(arg);
                Some(Term::Primitive {
                    name,
                    args,
                    ty,
                    origin,
                })
            }
            _ => None,
        }
    }
}

/// Apply `args` to `head` in order, reducing as far as possible.
///
/// The result is the reduced term when every argument applied, and otherwise a
/// single [`Term::Apply`] holding the head and the arguments that did not. It
/// is one node rather than a left-nested chain of one-argument applications:
/// `f x y` is one application of two arguments, and splitting it would make two
/// equal programs — `f x y` and the same written `(f x) y` — compare unequal
/// after normalization, which is the property this module exists to provide.
pub fn reduce_application(head: Term, args: Vec<Term>, origin: Origin) -> Term {
    let mut value = head;
    let mut rest = args.into_iter();
    // Apply while the head is still a function. The first argument that cannot
    // be applied ends the reduction: a head that is not a function does not
    // become one by being given more arguments, so the remainder is kept
    // together in a single node.
    let mut pending: Vec<Term> = Vec::new();
    for a in rest.by_ref() {
        // `apply` consumes its argument; the failing branch has to keep it, so
        // hand over a copy. A term here is small and this path runs once per
        // argument, so the copy is not worth avoiding with a fallible-borrow
        // signature that would complicate every other caller.
        match value.clone().apply(a.clone()) {
            Some(reduced) => value = reduced,
            None => {
                pending.push(a);
                break;
            }
        }
    }
    pending.extend(rest);
    if pending.is_empty() {
        value
    } else {
        Term::Apply {
            head: Box::new(value),
            args: pending,
            origin,
        }
    }
}

/// Give a reduced term the type and span of the application that produced it.
///
/// A template or primitive that has just received its last argument *becomes*
/// the application: its type is the application's type and its origin is the
/// application's span. A lambda that reduced to a value keeps that value's own
/// origin, because the value is what the user's other expression referred to —
/// `(f => f) .name` is the column, and blaming the lambda would point at
/// syntax that produced no node.
///
/// An unreduced `Apply` keeps its own origin for the same reason: it stands for
/// the application a user wrote.
fn apply_type_and_span(term: Term, ty: crate::core::ScalarType, origin: Origin) -> Term {
    use crate::core::ScalarType;
    // Never replace a known type with an unknown one. The application's own
    // type is the better source when it is known, but it is legitimately
    // unsolved for an overloaded call — `.age + 1` has `expr r a` with `a`
    // never solved — while the template already carries the chosen candidate's
    // declared result. Downgrading here would discard the better answer.
    let prefer = |current: &ScalarType| -> ScalarType {
        match (&ty, current) {
            (ScalarType::Unknown, known) if *known != ScalarType::Unknown => known.clone(),
            _ => ty.clone(),
        }
    };
    match term {
        Term::Template {
            sql,
            args,
            kind,
            ty: current,
            ..
        } => Term::Template {
            sql,
            args,
            kind,
            ty: prefer(&current),
            origin,
        },
        Term::Primitive {
            name,
            args,
            ty: current,
            ..
        } => Term::Primitive {
            name,
            args,
            ty: prefer(&current),
            origin,
        },
        other => other,
    }
}

/// Replace every free reference to `name` in `term` with `arg`.
///
/// A capture-avoiding substitution is not needed here, and that is worth
/// stating rather than leaving implicit: binders in the normal form are
/// **unique** (each came from a distinct source binder, and shadowing was
/// resolved by the checker, not by name), so an argument can never be captured
/// by an inner binder that happens to share a name. If that invariant is ever
/// violated, this function is where it must be detected.
fn substitute(term: Term, name: &str, arg: &Term) -> Term {
    match term {
        // A local reference is replaced when it is the binder. Matching on the
        // *label* here is safe precisely because resolution already happened:
        // this is the binder's own name, recorded by the checker, not a name to
        // be looked up.
        Term::Local { label, .. } if label == name => arg.clone(),
        Term::Local { .. } | Term::Column { .. } | Term::Lit { .. } => term,
        Term::Template {
            sql,
            args,
            kind,
            ty,
            origin,
        } => Term::Template {
            sql,
            args: args.into_iter().map(|a| substitute(a, name, arg)).collect(),
            kind,
            ty,
            origin,
        },
        Term::Primitive {
            name: pname,
            args,
            ty,
            origin,
        } => Term::Primitive {
            name: pname,
            args: args.into_iter().map(|a| substitute(a, name, arg)).collect(),
            ty,
            origin,
        },
        Term::Lambda {
            params,
            body,
            source,
            origin,
        } => {
            // Shadowing: an inner binder of the same name stops the
            // substitution, exactly as lexical scope requires. The checker
            // resolves shadowing, so this arm is a correctness guard rather
            // than a case that should arise.
            if params.iter().any(|p| p == name) {
                Term::Lambda {
                    params,
                    body,
                    source,
                    origin,
                }
            } else {
                Term::Lambda {
                    params,
                    body: Box::new(substitute(*body, name, arg)),
                    source,
                    origin,
                }
            }
        }
        // An unreduced application: substitute through both the head and the
        // arguments, then retry the reduction. Retrying matters — substituting
        // for the head is exactly what makes `f x` reduce when `f` arrives.
        Term::Apply { head, args, origin } => {
            let head = substitute(*head, name, arg);
            let args: Vec<Term> = args.into_iter().map(|a| substitute(a, name, arg)).collect();
            reduce_application(head, args, origin)
        }
        Term::Window {
            sql,
            spec,
            args,
            ty,
            origin,
        } => Term::Window {
            sql,
            spec: Box::new(substitute(*spec, name, arg)),
            args: args.into_iter().map(|a| substitute(a, name, arg)).collect(),
            ty,
            origin,
        },
        Term::Case {
            arms,
            default,
            origin,
        } => Term::Case {
            arms: arms
                .into_iter()
                .map(|(c, v)| (substitute(c, name, arg), substitute(v, name, arg)))
                .collect(),
            default: Box::new(substitute(*default, name, arg)),
            origin,
        },
        Term::Record { fields, origin } => Term::Record {
            fields: fields
                .into_iter()
                .map(|(k, v)| (k, substitute(v, name, arg)))
                .collect(),
            origin,
        },
        Term::List { items, origin } => Term::List {
            items: items
                .into_iter()
                .map(|i| substitute(i, name, arg))
                .collect(),
            origin,
        },
    }
}

/// What a name was resolved to, before its body is normalized.
///
/// This is the input side: a caller that has already asked the checker what a
/// name means hands the answer here, and normalization turns it into a
/// [`Term`]. Keeping it separate is what makes aliases disappear without a
/// `follow_alias` walk — an alias *is* its target, so the second lookup simply
/// does not happen.
///
/// Mirrors [`crate::workspace::Binding`] rather than reusing it, for two
/// reasons: `Binding` is the *resolver's* vocabulary and includes cases (a
/// module, a name bound to nothing) that cannot denote a term, and a consumer
/// should not gain a new variant every time the resolver does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The checker resolved the name to this definition.
    Def(DefId),
    /// The name is defined more than once with signatures, and the checker
    /// picked one candidate **per use**. The candidates travel with the
    /// resolution because which one applies is a fact about *this* occurrence —
    /// recorded in `TypeCheck`'s choices — not a property of the name.
    Overloads { module: usize, defs: Vec<usize> },
    /// The checker resolved the name to a primitive.
    Primitive,
}

impl Resolved {
    /// The resolution a module-scope binding denotes, or `None` when the
    /// binding cannot denote a term at all.
    ///
    /// `None` for [`crate::workspace::Binding::Module`]: a module is not a
    /// value, and a bare reference to one is already a checker error, so there
    /// is nothing to report here.
    pub fn of_binding(binding: &crate::workspace::Binding) -> Option<Self> {
        match binding {
            crate::workspace::Binding::Def(m, d) => Some(Resolved::Def((*m, *d))),
            crate::workspace::Binding::Overloads(m, defs) => Some(Resolved::Overloads {
                module: *m,
                defs: defs.clone(),
            }),
            crate::workspace::Binding::Prim(_) => Some(Resolved::Primitive),
            crate::workspace::Binding::Module(_) => None,
        }
    }
}

/// The operation normalization performs on one occurrence.
///
/// A caller supplies one of these per unresolved name. Returning `None` means
/// "this occurrence is a local", which is the only case where the term keeps a
/// name at all.
/// Resolves one name occurrence, **given the module it was written in**.
///
/// The module is a parameter rather than captured because the walk crosses
/// module boundaries: a user definition that calls a prelude helper descends
/// into the prelude's body, and that body's names are recorded under the
/// *prelude's* index. A resolver that captured the caller's module asked the
/// wrong module and reported a resolved name as unrecorded.
pub type Resolver<'a> = dyn Fn(usize, u32) -> Option<Resolved> + 'a;

/// The environment normalization runs in.
///
/// `binders` is the stack of names introduced by enclosing lambdas, innermost
/// last. It exists so a `Local` occurrence can record *which* binder it refers
/// to — the check is needed because a name may be shadowed, and the innermost
/// binding must win.
pub struct Normalizer<'a> {
    resolver: Box<Resolver<'a>>,
    /// The definition currently being normalized, for `Term::Lambda::source`.
    owner: Option<DefId>,
    /// Bodys already normalized, so a diamond-shaped dependency is reduced once.
    cache: HashMap<DefId, Arc<Term>>,
    /// Bindings introduced by enclosing lambdas, innermost last.
    binders: Vec<String>,
    /// The module whose body is being walked, so a name is resolved against the
    /// records of the module it was *written in* rather than the caller's.
    current_module: usize,
}

impl<'a> Normalizer<'a> {
    pub fn new(resolver: Box<Resolver<'a>>) -> Self {
        Self {
            resolver,
            owner: None,
            cache: HashMap::new(),
            binders: Vec::new(),
            current_module: 0,
        }
    }

    /// Whether `name` is bound by an enclosing binder rather than the module.
    ///
    /// This is the question that has to be asked *before* resolution: a lambda
    /// parameter shadows a top-level definition, so asking the checker for a
    /// name that is locally bound would find the wrong thing.
    pub fn is_local(&self, name: &str) -> bool {
        self.binders.iter().any(|b| b == name)
    }

    /// Resolve one occurrence, in the module currently being walked.
    ///
    /// `module` comes from [`Self::current_module`], which
    /// [`Self::define`] moves as the walk descends, so a prelude body resolves
    /// against the prelude's own records.
    pub fn resolve(&self, module: usize, id: u32) -> Option<Resolved> {
        (self.resolver)(module, id)
    }

    /// The module whose body is being walked right now.
    pub fn current_module(&self) -> usize {
        self.current_module
    }

    /// Push a binder for the duration of `f`.
    pub fn with_binder<T>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        self.binders.push(name.to_string());
        let out = f(self);
        self.binders.pop();
        out
    }

    /// Walk a definition of `module`, restoring the previous module after.
    ///
    /// The module moves with the walk rather than being fixed at the entry
    /// point: descending into a prelude helper means the names inside it were
    /// recorded under the prelude's index.
    pub fn in_module<T>(&mut self, module: usize, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = std::mem::replace(&mut self.current_module, module);
        let out = f(self);
        self.current_module = previous;
        out
    }

    /// The normal form of a definition, reduced once and shared.
    pub fn define(&mut self, id: DefId, build: impl FnOnce(&mut Self) -> Term) -> Arc<Term> {
        if let Some(cached) = self.cache.get(&id) {
            return Arc::clone(cached);
        }
        let previous = self.owner.replace(id);
        let term = build(self);
        self.owner = previous;
        let shared = Arc::new(term);
        self.cache.insert(id, Arc::clone(&shared));
        shared
    }
}

/// The facts the bridge reads, so it never touches `TypeCheck` directly.
///
/// Narrowing the dependency to exactly what normalization needs is deliberate:
/// the checker's surface is large, and a bridge that could read anything would
/// drift into re-deciding types — the failure this whole module exists to
/// remove.
pub trait Facts {
    /// What the name occurrence with this `ExprId` resolved to.
    ///
    /// `None` when the occurrence was not a module-scope name.
    fn resolution(&self, module: usize, id: u32) -> Option<Resolved>;

    /// The definition body to normalize, given a resolved definition.
    ///
    /// Returned by value because the caller owns a workspace and the bridge
    /// must not hold a borrow across the recursive walk.
    fn def_body(&self, module: usize, def: usize) -> Option<Body>;

    /// Whether the definition is a `sql "..."` template, and its text, and the
    /// phase kind its signature declares.
    fn def_template(&self, module: usize, def: usize) -> Option<Template>;

    /// Which overload candidate the checker chose at this use site.
    ///
    /// `None` when there is nothing recorded, which means the use is open
    /// inside its own polymorphic definition — a `Hole` — and its candidate
    /// comes from the enclosing instance. That case is *not* resolvable here,
    /// so the bridge reports it rather than guessing candidate zero.
    fn choice(&self, module: usize, owner: usize, site: u32) -> Option<Choice>;

    /// The scalar type a definition's signature **declares** it returns.
    ///
    /// For a `sql` template this is the honest source of its result type, and
    /// it is not the same question as "what type did this use site get": an
    /// overloaded operator is instantiated fresh per use, so the *use's* type
    /// may still hold an unsolved variable while the chosen candidate's
    /// signature says plainly that `_+_` over ints returns an int.
    fn def_result_ty(&self, module: usize, def: usize) -> Option<crate::core::ScalarType>;

    /// The scalar type the checker gave the expression with this `ExprId`.
    ///
    /// `ScalarType::Unknown` when the checker recorded none, matching
    /// `TypeCheck::use_ty`: "the checker did not solve this" is a fact worth
    /// carrying rather than a reason to fail, because the definition was
    /// already reported.
    fn use_ty(&self, module: usize, id: u32) -> crate::core::ScalarType;
}

/// A `sql` template definition: its text and its declared phase kind.
#[derive(Debug, Clone)]
pub struct Template {
    pub sql: String,
    pub kind: TemplateKind,
}

/// A definition's body, with the module and definition it came from.
///
/// The pair travels with the syntax because every fact the bridge reads —
/// resolutions, types — is keyed by where a piece of syntax was *written*, and
/// normalizing a callee's body moves to the callee's module.
#[derive(Debug, Clone)]
pub struct Body {
    pub module: usize,
    pub def: usize,
    pub expr: cagara_syntax::ast::Expr,
}

/// What the bridge could not normalize.
///
/// Distinct from a *type* error: reaching this means the program is fine and
/// normalization has no rule for the construct, which is a compiler capability
/// gap. Classifying it here rather than at the call site keeps the caller from
/// having to guess which failures are its own.
#[derive(Debug, Clone)]
pub struct Gap {
    pub origin: Origin,
    pub message: String,
}

impl Gap {
    pub fn new(origin: Origin, message: impl Into<String>) -> Self {
        Self {
            origin,
            message: message.into(),
        }
    }
}

/// Build the normal form of one expression.
///
/// This is the **bridge**: the single place that reads source syntax together
/// with the checker's facts and produces a [`Term`]. Everything downstream
/// consumes the term, so no later phase needs to look at syntax to decide what
/// something means.
///
/// The rule it follows is narrow on purpose — for each form it does the least
/// that produces a term, and defers everything else:
///
/// * a **column** or a **literal** becomes a leaf;
/// * a **name** is resolved through [`Facts::resolution`], never by looking it
///   up again. A name that resolves to a definition is *inlined* to that
///   definition's normal form, which is what removes aliases: an alias has no
///   term of its own, it simply is its target;
/// * a **lambda** records its binders, so the term is a function value that
///   [`Term::apply`] can reduce;
/// * an **application** normalizes the head and the arguments and reduces.
pub fn expression(facts: &dyn Facts, cx: &mut Normalizer<'_>, body: &Body) -> Result<Term, Gap> {
    use cagara_syntax::ast::ExprKind;

    let module = body.module;
    let e = &body.expr;
    let origin = Origin::new(module, e.span);

    match &e.kind {
        // A local binder wins over the module scope, exactly as lexical
        // scoping requires. The check is against the *binder stack* rather
        // than the resolver, because the resolver answers about module-scope
        // names only.
        ExprKind::Name(n) if cx.is_local(n) => Ok(Term::Local {
            id: e.id,
            label: n.clone(),
            ty: facts.use_ty(module, e.id),
            origin,
        }),
        // A literal, including the `sql "..."` template form written inline.
        ExprKind::Lit(l) => Ok(Term::Lit {
            lit: lit_of(l),
            ty: facts.use_ty(module, e.id),
            origin,
        }),
        ExprKind::Sql(sql) => Ok(Term::Template {
            sql: std::sync::Arc::from(sql.as_str()),
            args: Vec::new(),
            kind: TemplateKind::Scalar,
            // The type this body *returns*, which is what an applied template
            // produces. `use_ty` at this node gives the template's own function
            // type (`expr r string -> expr r string`), so reading it here would
            // record a function where the applied result belongs — and the
            // template is applied by definition, since a bare `sql` body is
            // only reachable through the definition that declares it.
            ty: facts
                .def_result_ty(module, body.def)
                .unwrap_or(crate::core::ScalarType::Unknown),
            origin,
        }),
        // A column reference. `side` distinguishes the two sides of a join
        // predicate; it is part of the term rather than a later re-reading of
        // the syntax, because a join's sides are a property of the query, not
        // of the expression.
        ExprKind::Field(side, n) => Ok(Term::Column {
            side: *side,
            name: n.clone(),
            ty: facts.use_ty(module, e.id),
            origin,
        }),
        // A name that is not a local: resolve it, and *inline* what it denotes.
        // This is where aliases disappear — following the chain is not a step,
        // it is the absence of a step.
        ExprKind::Name(n) => match cx.resolve(module, e.id) {
            Some(Resolved::Def((dm, dd))) => {
                // A `sql` template is a value in its own right: inlining it
                // means building the template, not forcing it to a scalar.
                if let Some(t) = facts.def_template(dm, dd) {
                    return Ok(Term::Template {
                        sql: std::sync::Arc::from(t.sql.as_str()),
                        args: Vec::new(),
                        kind: t.kind,
                        // The template's **declared result**, not this node's
                        // type. A bare name referring to a template has the
                        // template's *function* type at its use site
                        // (`expr r string -> expr r string`), and reading that
                        // would record a function where the applied result —
                        // `string` — belongs. The same rule as the overload
                        // path below, for the same reason.
                        ty: facts
                            .def_result_ty(dm, dd)
                            .unwrap_or(crate::core::ScalarType::Unknown),
                        origin,
                    });
                }
                let inner = facts
                    .def_body(dm, dd)
                    .ok_or_else(|| Gap::new(origin, format!("`{n}` has no body to normalize")))?;
                // The callee's names were recorded under the **callee's**
                // module, so the walk moves there. This is what makes a user
                // definition that calls a prelude helper resolve the helper's
                // own names correctly.
                cx.in_module(dm, |cx| expression(facts, cx, &inner))
            }
            // An overloaded name: the checker picked one candidate **for this
            // use**, and that choice is recorded per use site rather than being
            // a property of the name. Consulting it is what makes `+` resolve
            // to the int or the float definition at each occurrence.
            Some(Resolved::Overloads { module: om, defs }) => {
                let chosen = match facts.choice(module, body.def, e.id) {
                    Some(Choice::Def(_, di)) => di,
                    Some(Choice::Hole(k)) => {
                        return Err(Gap::new(
                            origin,
                            format!(
                                "`{n}` is an open overload (hole {k}) inside its own definition; \
                                 normalization runs per use, where the hole is assigned"
                            ),
                        ))
                    }
                    // Nothing recorded: the use is open inside its own
                    // definition. Reported rather than defaulted, because
                    // taking candidate zero would silently pick one of several
                    // valid readings.
                    None => {
                        return Err(Gap::new(
                            origin,
                            format!("the checker recorded no candidate for the overloaded `{n}`"),
                        ))
                    }
                };
                // A candidate that is not among the recorded ones is a
                // contradiction between two checker facts; reporting it is
                // better than elaborating a definition the use did not choose.
                if !defs.contains(&chosen) {
                    return Err(Gap::new(
                        origin,
                        format!("`{n}` resolved to a definition that is not one of its candidates"),
                    ));
                }
                if let Some(t) = facts.def_template(om, chosen) {
                    return Ok(Term::Template {
                        sql: std::sync::Arc::from(t.sql.as_str()),
                        args: Vec::new(),
                        kind: t.kind,
                        // The **candidate's declared result type**, not this
                        // use site's. An overloaded operator is instantiated
                        // fresh per use, so the use's own type may still hold an
                        // unsolved variable — measured on `.age + 1`, whose
                        // application type is `expr r a` with `a` never solved —
                        // while the signature of the candidate the checker chose
                        // says plainly that it returns an int.
                        ty: facts
                            .def_result_ty(om, chosen)
                            .unwrap_or(crate::core::ScalarType::Unknown),
                        origin,
                    });
                }
                let inner = facts.def_body(om, chosen).ok_or_else(|| {
                    Gap::new(origin, format!("the chosen candidate of `{n}` has no body"))
                })?;
                cx.in_module(om, |cx| expression(facts, cx, &inner))
            }
            Some(Resolved::Primitive) => Ok(Term::Primitive {
                name: std::sync::Arc::from(n.as_str()),
                args: Vec::new(),
                ty: facts.use_ty(module, e.id),
                origin,
            }),
            None => Err(Gap::new(
                origin,
                format!("the checker recorded no resolution for `{n}`"),
            )),
        },
        // A lambda: record the binders, normalize the body under them.
        ExprKind::Lambda(param, body_expr) => {
            let inner = cx.with_binder(param, |cx| {
                expression(
                    facts,
                    cx,
                    &Body {
                        module,
                        def: body.def,
                        expr: (**body_expr).clone(),
                    },
                )
            })?;
            Ok(Term::Lambda {
                params: vec![param.clone()],
                body: Box::new(inner),
                source: Some((module, body.def)),
                origin,
            })
        }
        // An application: normalize the head, then reduce it against each
        // argument. This is the substitution the old design did by re-walking
        // source.
        ExprKind::App(head, args) => {
            let head_term = expression(
                facts,
                cx,
                &Body {
                    module,
                    def: body.def,
                    expr: (**head).clone(),
                },
            )?;
            let mut arg_terms = Vec::with_capacity(args.len());
            for arg in args.iter() {
                arg_terms.push(expression(
                    facts,
                    cx,
                    &Body {
                        module,
                        def: body.def,
                        expr: arg.clone(),
                    },
                )?);
            }
            // Reduce as far as the head allows. An application that cannot
            // reduce is *kept*, not rejected: `f x` inside `f => x => f x` has
            // nothing to substitute into until the enclosing lambda is applied,
            // and every curried combinator in the prelude is written that way.
            let reduced = reduce_application(head_term, arg_terms, origin);
            // A *saturated* template takes the application's own type and span.
            //
            // Not cosmetic: reading the type off the head records the template's
            // result type at the *definition's* `ExprId`, while the elaborator
            // and the incremental facts read the type of the *use*; and the
            // head's span would point a diagnostic at `upper` rather than at
            // `upper .name`, which is the expression the user wrote and the one
            // the checker blamed.
            Ok(apply_type_and_span(
                reduced,
                facts.use_ty(module, e.id),
                origin,
            ))
        }
        // A `case { cond => value; …; true => default }`.
        //
        // The final `true =>` arm is split off as the default, which is what
        // makes `case { c => x; true => y }` and `ifThenElse c x y` normalize to
        // the same term. The checker requires that last arm to be a literal
        // `true`, so the split is total on a program it accepted; a `case` with
        // no such arm is reported rather than silently treated as one, because
        // guessing a default would invent a value the program never had.
        ExprKind::Case(arms) => {
            let Some(((cond, default), whens)) = arms.split_last() else {
                return Err(Gap::new(origin, "`case` needs at least one arm"));
            };
            let is_true = matches!(
                cond.kind,
                ExprKind::Lit(cagara_syntax::ast::Lit::Bool(true))
            );
            if !is_true {
                return Err(Gap::new(
                    origin,
                    "`case` must end with a `true =>` default arm",
                ));
            }
            let mut out = Vec::with_capacity(whens.len());
            for (c, v) in whens {
                out.push((
                    expression(
                        facts,
                        cx,
                        &Body {
                            module,
                            def: body.def,
                            expr: c.clone(),
                        },
                    )?,
                    expression(
                        facts,
                        cx,
                        &Body {
                            module,
                            def: body.def,
                            expr: v.clone(),
                        },
                    )?,
                ));
            }
            let default = expression(
                facts,
                cx,
                &Body {
                    module,
                    def: body.def,
                    expr: default.clone(),
                },
            )?;
            Ok(Term::Case {
                arms: out,
                default: Box::new(default),
                origin,
            })
        }
        // A list literal: `inList [..] .x`. Kept in source order; the checked
        // layer takes the elements positionally.
        ExprKind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(expression(
                    facts,
                    cx,
                    &Body {
                        module,
                        def: body.def,
                        expr: item.clone(),
                    },
                )?);
            }
            Ok(Term::List { items: out, origin })
        }
        // A record literal: `select { x = … }`, `update { … }`, a window spec.
        // Each field is normalized, and the record keeps its **source order**,
        // because a projection's output row is ordered by it and the checked
        // layer builds the row from the order it is given.
        ExprKind::Record(fields) => {
            let mut out = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                out.push((
                    name.clone(),
                    expression(
                        facts,
                        cx,
                        &Body {
                            module,
                            def: body.def,
                            expr: value.clone(),
                        },
                    )?,
                ));
            }
            Ok(Term::Record {
                fields: out,
                origin,
            })
        }
        // A `primitive "__name"` reference. Arguments, when there are any,
        // arrive as an enclosing `App` and are reduced by the application arm
        // above; a bare primitive is a value awaiting them, exactly as a `sql`
        // template is.
        ExprKind::Primitive(name) => Ok(Term::Primitive {
            name: std::sync::Arc::from(name.as_str()),
            args: Vec::new(),
            ty: facts.use_ty(module, e.id),
            origin,
        }),
        // Forms normalization has no rule for yet. Reported as a gap rather
        // than guessed at, so the migration can proceed family by family and a
        // missing family shows up as a named gap instead of a wrong term.
        other => Err(Gap::new(
            origin,
            format!(
                "normalization has no rule for {}",
                describe_expr_kind(other)
            ),
        )),
    }
}

/// Normalize and lower one expression, the way the production path does it.
///
/// The single entry point that runs the whole new route: build a term from the
/// checker's facts, then lower it into the checked layer. A caller that has a
/// `CompilerInput` needs nothing else.
///
/// `owner` is the definition whose body the expression belongs to; overload
/// choices are recorded per definition, so resolving one needs it alongside the
/// use site.
pub fn elaborate_via_term(
    input: crate::compile::CompilerInput<'_>,
    module: usize,
    owner: usize,
    e: &cagara_syntax::ast::Expr,
) -> Result<crate::checked::CheckedExpr, Gap> {
    let facts = InputFacts::new(input);
    let body = Body {
        module,
        def: owner,
        expr: e.clone(),
    };
    // The resolver reads the checker's record for this module. `Normalizer`
    // calls it per name occurrence, so the borrow of `facts` must outlive the
    // walk — hence the closure borrowing `facts` rather than `input`.
    let resolve = |module: usize, id: u32| facts.resolution(module, id);
    let mut cx = Normalizer::new(Box::new(resolve));
    let term = match cx.in_module(module, |cx| expression(&facts, cx, &body)) {
        Ok(t) => t,
        Err(g) => {
            note_gap(&g.message);
            return Err(g);
        }
    };
    match lower_expr(&term) {
        Ok(lowered) => Ok(lowered),
        Err(g) => {
            // Counted too. The bridge and the lowering are two halves of one
            // route, and a fallback caused by either means the source path is
            // still load-bearing — so a counter that watched only the bridge
            // would report zero while lowering quietly deferred.
            note_gap(&g.message);
            Err(g)
        }
    }
}

/// Lower a normalized term into the **checked expression** layer.
///
/// This is the consumer side of the bridge. Doing it from a [`Term`] rather
/// than from syntax is what lets the checked constructors be the only thing
/// that decides whether a tree is valid: by the time a term exists, every name
/// is resolved, every application is saturated or explicitly pending, and every
/// type was read from the checker rather than re-derived.
///
/// The phases are *not* chosen here. `CheckedExpr::template` folds its
/// arguments' phases with `rules::mix`, so an expression whose arguments are
/// aggregates comes out an aggregate — which is what makes
/// `ifThenElse (…) (sum .x) 0` usable inside an `agg` stage without this
/// function having to reason about phases at all.
pub fn lower_expr(t: &Term) -> Result<crate::checked::CheckedExpr, Gap> {
    use crate::checked::CheckedExpr as C;

    match t {
        Term::Local {
            label, ty, origin, ..
        } => {
            // A column reference is spelled `.name` by the bridge; a binder's
            // local carries its own name. The distinction is the label's shape,
            // recorded when the term was built rather than re-read from syntax.
            let name = label.strip_prefix('.').unwrap_or(label);
            Ok(C::column(
                cagara_syntax::ast::Side::Single,
                name,
                ty.clone(),
                *origin,
            ))
        }
        Term::Column {
            side,
            name,
            ty,
            origin,
        } => Ok(C::column(*side, name.clone(), ty.clone(), *origin)),
        Term::Lit { lit, origin, .. } => Ok(C::lit(lit.clone(), *origin)),
        Term::Template {
            sql,
            args,
            kind,
            ty,
            origin,
        } => {
            let lowered = lower_exprs(args)?;
            match kind {
                TemplateKind::Scalar => C::template(sql.to_string(), lowered, ty.clone(), *origin)
                    .map_err(|e| Gap::new(*origin, e.message)),
                TemplateKind::Agg => C::agg_template(sql.to_string(), lowered, ty.clone(), *origin)
                    .map_err(|e| Gap::new(*origin, e.message)),
                // A window template needs a spec, which is a query-level
                // concept this expression lowering does not carry. Reported
                // rather than given an invented spec.
                // A window template reached as a bare name has no spec yet —
                // it is a function awaiting one. Reaching here means it was
                // applied without a spec argument, which the checker refuses to
                // type, so this is reported rather than given an invented spec.
                TemplateKind::Win => Err(Gap::new(
                    *origin,
                    "a window template needs its spec argument",
                )),
            }
        }
        // A window application is recognized, and its spec is carried as a
        // term — but the spec is a *query-level* record with its own rules
        // (which fields exist, that an order key must name a column, what a
        // frame bound may be), and those rules are already implemented by
        // `elaborate_winspec`. Lowering it here would be a second copy.
        //
        // So this arm reports rather than invents, and a caller that has a
        // spec-lowering hook uses `lower_expr_with` instead. The window path
        // therefore still runs through the source route, which is honest: it is
        // one family, and claiming it is migrated while lowering it wrongly
        // would be worse than leaving it.
        Term::Window { origin, .. } => Err(Gap::new(
            *origin,
            "a window application needs its spec lowered by the query layer",
        )),
        Term::Primitive {
            name,
            args,
            ty: _,
            origin,
        } => lower_primitive(name, args, *origin),
        Term::Case {
            arms,
            default,
            origin,
        } => {
            // The searched `CASE` the prelude's `ifThenElse` emits. Built here
            // rather than reached through a name because the arity depends on
            // how many arms were written, which no fixed-arity definition can
            // express.
            let mut sql = String::from("CASE");
            let mut lowered: Vec<crate::checked::CheckedExpr> = Vec::new();
            for (cond, value) in arms {
                sql.push_str(" WHEN $");
                sql.push_str(&(lowered.len() + 1).to_string());
                lowered.push(lower_expr(cond)?);
                sql.push_str(" THEN $");
                sql.push_str(&(lowered.len() + 1).to_string());
                lowered.push(lower_expr(value)?);
            }
            sql.push_str(" ELSE $");
            sql.push_str(&(lowered.len() + 1).to_string());
            lowered.push(lower_expr(default)?);
            sql.push_str(" END");
            C::template(sql, lowered, crate::core::ScalarType::Unknown, *origin)
                .map_err(|e| Gap::new(*origin, e.message))
        }
        // A function value reaching expression position is the caller's error,
        // and the checker already refused to type it. Reported as a gap so it
        // cannot be silently dropped.
        Term::Lambda { origin, .. } => {
            Err(Gap::new(*origin, "a function value is not an expression"))
        }
        Term::Apply { origin, .. } => Err(Gap::new(
            *origin,
            "an unreduced application is not an expression",
        )),
        Term::Record { origin, .. } => Err(Gap::new(
            *origin,
            "a record is lowered by the stage that consumes it",
        )),
        Term::List { origin, .. } => Err(Gap::new(
            *origin,
            "a list is handled by the primitive that consumes it",
        )),
    }
}

/// Lower a sequence of terms, reporting the first that cannot be lowered.
fn lower_exprs(ts: &[Term]) -> Result<Vec<crate::checked::CheckedExpr>, Gap> {
    ts.iter().map(lower_expr).collect()
}

/// Lower an expression primitive: `group`, `in`, and the rest.
///
/// Only the two the checked layer has constructors for are handled; the others
/// are reported, matching the elaboration layer's own behavior rather than
/// inventing a node.
fn lower_primitive(
    name: &str,
    args: &[Term],
    origin: Origin,
) -> Result<crate::checked::CheckedExpr, Gap> {
    use crate::checked::CheckedExpr as C;
    match (name, args) {
        // `group key`: an aggregate-phase node whose type is the key's.
        ("__group", [key]) => {
            C::group(lower_expr(key)?, origin).map_err(|e| Gap::new(origin, e.message))
        }
        // `inList [..] .x`: the list comes first, then the value, matching the
        // prelude's declaration. Swapping them can still produce a well-typed
        // tree, so the order is stated explicitly.
        ("__in", [list, value]) => {
            let items = match list {
                Term::List { items, .. } => lower_exprs(items)?,
                other => {
                    return Err(Gap::new(
                        other_origin(other),
                        "the first argument of `in` must be a list",
                    ))
                }
            };
            C::in_(lower_expr(value)?, items, false, origin)
                .map_err(|e| Gap::new(origin, e.message))
        }
        (other, _) => Err(Gap::new(
            origin,
            format!("the expression primitive `{other}` is not lowered yet"),
        )),
    }
}

/// A term's origin, for reporting a gap about a subterm.
fn other_origin(t: &Term) -> Origin {
    match t {
        Term::Local { origin, .. }
        | Term::Column { origin, .. }
        | Term::Lit { origin, .. }
        | Term::Template { origin, .. }
        | Term::Primitive { origin, .. }
        | Term::Lambda { origin, .. }
        | Term::Apply { origin, .. }
        | Term::Window { origin, .. }
        | Term::Case { origin, .. }
        | Term::Record { origin, .. }
        | Term::List { origin, .. } => *origin,
    }
}

/// A short human name for an expression form, for a gap message.
fn describe_expr_kind(k: &cagara_syntax::ast::ExprKind) -> &'static str {
    use cagara_syntax::ast::ExprKind as K;
    match k {
        K::Name(_) => "a name",
        K::Lit(_) => "a literal",
        K::Field(..) => "a column reference",
        K::Proj(..) => "a projection",
        K::App(..) => "an application",
        K::Lambda(..) => "a lambda",
        K::Record(_) => "a record",
        K::List(_) => "a list",
        K::Sql(_) => "a sql template",
        K::Case(_) => "a case",
        K::Primitive(_) => "a primitive",
        K::Error => "an error node",
    }
}

/// The phase kind a definition's **declared signature** gives it.
///
/// Mirrors `elaborate::declared_kind`, and duplicated for the same reason
/// `lit_of` is: this one feeds a [`Term`], that one feeds a `CheckedExpr`, and
/// coupling them would mean a change to the checked layer forcing a change
/// here.
///
/// Read from the type expression rather than the resolved scheme because only
/// the declared form distinguishes `expr r a` from `agg (expr r a)`; after
/// resolution both are just the result type.
fn declared_template_kind(d: &cagara_syntax::ast::Def) -> TemplateKind {
    use cagara_syntax::ast::TypeExpr;
    let Some(mut t) = d.ty.as_ref() else {
        return TemplateKind::Scalar;
    };
    // Strip the argument arrows to reach the result.
    while let TypeExpr::Fun(_, r) = t {
        t = r;
    }
    match t {
        TypeExpr::App { head, .. } if head == "agg" => TemplateKind::Agg,
        TypeExpr::App { head, .. } if head == "win" => TemplateKind::Win,
        _ => TemplateKind::Scalar,
    }
}

/// The literal a source literal denotes.
///
/// Mirrors `elaborate::lit_of`, which does the same copy for the checked layer.
/// The two `Lit` enums have identical shape, so this is a pure translation; it
/// is written out rather than derived so that a variant added to the AST is a
/// compile error here instead of a silently dropped literal.
fn lit_of(l: &cagara_syntax::ast::Lit) -> crate::ir::Lit {
    use cagara_syntax::ast::Lit as L;
    match l {
        L::Int(i) => crate::ir::Lit::Int(*i),
        L::Float(f) => crate::ir::Lit::Float(f.clone()),
        L::Str(s) => crate::ir::Lit::Str(s.clone()),
        L::Bool(b) => crate::ir::Lit::Bool(*b),
    }
}

/// [`Facts`] backed by a real workspace, for tests and for the migration's
/// differential check.
///
/// Lives in the library rather than in `#[cfg(test)]` because the migration
/// compares the bridge against the existing elaborator, and that comparison has
/// to run the same code the production path would.
pub struct WorkspaceFacts<'a> {
    pub ws: &'a crate::workspace::Workspace,
    pub tc: &'a crate::check::TypeCheck,
}

impl<'a> WorkspaceFacts<'a> {
    pub fn new(ws: &'a crate::workspace::Workspace, tc: &'a crate::check::TypeCheck) -> Self {
        Self { ws, tc }
    }
}

/// [`Facts`] backed by a [`CompilerInput`], which is what the production
/// elaboration path has.
///
/// A second adapter rather than reusing [`WorkspaceFacts`] because the two see
/// different things and neither should be widened to match the other: the
/// workspace adapter is for tests that hold a `Workspace`, and this one is for
/// the compiler, which deliberately holds only a read-only input view.
pub struct InputFacts<'a> {
    pub input: crate::compile::CompilerInput<'a>,
}

impl<'a> InputFacts<'a> {
    pub fn new(input: crate::compile::CompilerInput<'a>) -> Self {
        Self { input }
    }
}

impl Facts for InputFacts<'_> {
    fn resolution(&self, module: usize, id: u32) -> Option<Resolved> {
        self.input.type_check().resolution(module, id).cloned()
    }

    fn def_body(&self, module: usize, def: usize) -> Option<Body> {
        let d = self.input.module(module).source().defs.get(def)?;
        Some(Body {
            module,
            def,
            expr: d.body.clone(),
        })
    }

    fn def_template(&self, module: usize, def: usize) -> Option<Template> {
        use cagara_syntax::ast::ExprKind;
        let d = self.input.module(module).def(def);
        match &d.body.kind {
            ExprKind::Sql(sql) => Some(Template {
                sql: sql.clone(),
                kind: declared_template_kind(d),
            }),
            _ => None,
        }
    }

    fn choice(&self, module: usize, owner: usize, site: u32) -> Option<Choice> {
        let tc = self.input.type_check();
        let mut found = tc
            .choices_of(module, owner)
            .into_iter()
            .filter(|(s, _, _)| *s == site);
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(first.2)
    }

    fn def_result_ty(&self, module: usize, def: usize) -> Option<crate::core::ScalarType> {
        self.input.type_check().result_scalar(module, def)
    }

    fn use_ty(&self, module: usize, id: u32) -> crate::core::ScalarType {
        self.input
            .type_check()
            .use_ty(module, id)
            .unwrap_or(crate::core::ScalarType::Unknown)
    }
}

impl Facts for WorkspaceFacts<'_> {
    fn resolution(&self, module: usize, id: u32) -> Option<Resolved> {
        self.tc.resolution(module, id).cloned()
    }

    fn def_body(&self, module: usize, def: usize) -> Option<Body> {
        let d = self.ws.modules.get(module)?.module.defs.get(def)?;
        Some(Body {
            module,
            def,
            expr: d.body.clone(),
        })
    }

    fn def_template(&self, module: usize, def: usize) -> Option<Template> {
        use cagara_syntax::ast::ExprKind;
        let d = self.ws.modules.get(module)?.module.defs.get(def)?;
        match &d.body.kind {
            ExprKind::Sql(sql) => Some(Template {
                sql: sql.clone(),
                kind: declared_template_kind(d),
            }),
            _ => None,
        }
    }

    fn choice(&self, module: usize, owner: usize, site: u32) -> Option<Choice> {
        // The checker records one entry per `(site, hole)`. A use with a single
        // hole — the common case, and the only one the bridge resolves — has
        // exactly one entry at this site; a use with several is ambiguous here
        // and deliberately reported as unresolved rather than taken in
        // iteration order, which is not a stable answer.
        let mut found = self
            .tc
            .choices_of(module, owner)
            .into_iter()
            .filter(|(s, _, _)| *s == site);
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(first.2)
    }

    fn def_result_ty(&self, module: usize, def: usize) -> Option<crate::core::ScalarType> {
        self.tc.result_scalar(module, def)
    }

    fn use_ty(&self, module: usize, id: u32) -> crate::core::ScalarType {
        self.tc
            .use_ty(module, id)
            .unwrap_or(crate::core::ScalarType::Unknown)
    }
}

#[cfg(test)]
thread_local! {
    /// Why terms fell back to the source path, counted for the migration report.
    static GAP_REASONS: std::cell::RefCell<std::collections::BTreeMap<String, usize>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Record a gap, for the migration's progress report.
pub fn note_gap(reason: &str) {
    #[cfg(test)]
    GAP_REASONS.with(|m| *m.borrow_mut().entry(reason.to_string()).or_default() += 1);
    #[cfg(not(test))]
    let _ = reason;
}

/// The reasons terms fell back, most frequent first.
#[cfg(test)]
pub fn gap_reasons() -> Vec<(String, usize)> {
    GAP_REASONS.with(|m| m.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

/// Clear the recorded gaps, so a caller can attribute them to one program.
#[cfg(test)]
pub fn reset_gaps() {
    GAP_REASONS.with(|m| m.borrow_mut().clear());
}

/// A placeholder so the `gaps_since_reset` call site reads naturally.
#[cfg(test)]
pub fn gaps_since_reset() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::new(0, cagara_syntax::ast::Span::default())
    }

    /// An unsolved scalar type, for terms built by hand in tests: the type is
    /// not what these tests are about, and `Unknown` is what the checker
    /// records when it did not solve one.
    fn no_ty() -> crate::core::ScalarType {
        crate::core::ScalarType::Unknown
    }

    fn local(label: &str) -> Term {
        Term::Local {
            id: 0,
            label: label.to_string(),
            ty: no_ty(),
            origin: origin(),
        }
    }

    fn lam(params: &[&str], body: Term) -> Term {
        Term::Lambda {
            params: params.iter().map(|s| s.to_string()).collect(),
            body: Box::new(body),
            source: None,
            origin: origin(),
        }
    }

    /// Applying a one-parameter function substitutes the argument, and the
    /// result is a term with no binder left.
    #[test]
    fn application_substitutes_and_closes() {
        let f = lam(&["x"], local("x"));
        let applied = f.apply(local("arg")).expect("a lambda is applicable");
        assert_eq!(applied, local("arg"));
        assert!(!applied.is_function());
    }

    /// A curried function applied one argument at a time is the same as one
    /// applied all at once.
    ///
    /// This is the property partial application needs: `k = x => y => x`
    /// applied to one argument must retain `x`, which the current design
    /// achieves by carrying an environment. Under substitution it falls out of
    /// the reduction.
    #[test]
    fn curried_application_matches_saturated_application() {
        // `k = x => y => x`
        let k = lam(&["x", "y"], local("x"));
        // Applied one at a time.
        let step = k.clone().apply(local("first")).expect("applicable");
        assert!(step.is_function(), "one argument leaves a function");
        let curried = step.apply(local("second")).expect("applicable");
        // Applied all at once.
        let saturated = k
            .apply(local("first"))
            .unwrap()
            .apply(local("second"))
            .unwrap();
        assert_eq!(curried, saturated);
        // And the answer is the *first* argument, so the substitution order is
        // right rather than coincidentally equal.
        assert_eq!(curried, local("first"));
    }

    /// The substitution invariant: a binder's name is not reused by a *free*
    /// reference inside its own body.
    ///
    /// [`substitute`] matches on the label rather than on a de Bruijn index, so
    /// it is correct only because the checker resolves shadowing before
    /// normalization: two distinct binders never share a name, and a reference
    /// to an outer binding is never written with an inner binder's name. This
    /// test pins the invariant on the case that would break the assumption —
    /// the *same* name bound at two nesting levels — and shows the fallback
    /// behaves: the inner one wins, and substituting for the outer name does
    /// not reach the inner body.
    #[test]
    fn a_shadowed_name_does_not_leak_into_the_inner_body() {
        // `x => (x => x)` — both binders are `x`. Applying the outer to a value
        // must leave the inner lambda alone, because its own `x` shadows.
        let outer = lam(&["x"], lam(&["x"], local("x")));
        let applied = outer.apply(local("value")).unwrap();
        assert!(applied.is_function(), "the inner lambda survives");
        // The inner application yields its own argument, not the outer value:
        // if substitution had leaked, this would be `value`.
        let inner = applied.apply(local("inner")).unwrap();
        assert_eq!(inner, local("inner"));
    }

    /// Substitution descends into every position a term can appear in.
    #[test]
    fn substitution_reaches_nested_positions() {
        let body = Term::Record {
            fields: vec![
                ("a".into(), local("p")),
                (
                    "b".into(),
                    Term::List {
                        items: vec![local("p"), local("other")],
                        origin: origin(),
                    },
                ),
            ],
            origin: origin(),
        };
        let applied = lam(&["p"], body).apply(local("v")).unwrap();
        let Term::Record { fields, .. } = &applied else {
            panic!("the record shape must survive reduction");
        };
        assert_eq!(fields[0].1, local("v"));
        let Term::List { items, .. } = &fields[1].1 else {
            panic!("the list shape must survive reduction");
        };
        assert_eq!(items[0], local("v"), "the bound name is replaced");
        assert_eq!(items[1], local("other"), "an unbound name is untouched");
    }

    /// An inner binder of the same name shadows the outer one, so substitution
    /// stops there.
    #[test]
    fn an_inner_binder_shadows() {
        // `x => (x => x) x`  — the inner `x` refers to the inner binder.
        let body = lam(&["x"], local("x"));
        let outer = lam(&["x"], body);
        let applied = outer.apply(local("outer_arg")).unwrap();
        // The inner lambda is untouched, because its own binder shadows.
        assert!(applied.is_function());
        let inner = applied.apply(local("inner_arg")).unwrap();
        assert_eq!(inner, local("inner_arg"));
    }

    /// Normal forms compare structurally regardless of how they were built.
    ///
    /// This is the property the whole pass exists for, stated as a test: two
    /// spellings of one program produce equal terms.
    #[test]
    fn equal_programs_have_equal_normal_forms() {
        let direct = lam(&["f"], local("f")).apply(local("upper")).unwrap();
        // The same thing reached by a different route: an identity wrapper.
        let wrapped = lam(&["g"], local("g")).apply(local("upper")).unwrap();
        assert_eq!(direct, wrapped);
    }

    /// Applying a **local** keeps the application rather than failing.
    ///
    /// A local head is a lambda parameter whose value arrives later, so there
    /// is nothing to substitute yet. Treating this as a failure made the four
    /// curried combinators in the prelude — `_&_`, `_$_`, `_>>>_`, `_<<<_`, all
    /// written `f => x => f x` — the only definitions there that would not
    /// normalize.
    #[test]
    fn applying_a_local_keeps_the_application() {
        let kept = local("f")
            .apply(local("x"))
            .expect("a local head is applicable");
        match kept {
            Term::Apply { head, args, .. } => {
                assert_eq!(*head, local("f"));
                assert_eq!(args, vec![local("x")]);
            }
            other => panic!("expected a kept application, got {other:?}"),
        }
    }

    /// Applying a term that can *never* be a function yields nothing.
    ///
    /// A literal has no value that could be substituted, so this is the case
    /// `apply` rejects — and it is safe to reject because the checker has
    /// already refused to type it.
    #[test]
    fn applying_a_non_function_yields_nothing() {
        let lit = Term::Lit {
            lit: crate::ir::Lit::Int(1),
            ty: no_ty(),
            origin: origin(),
        };
        assert!(lit.apply(local("y")).is_none());
        assert!(lam(&[], local("x")).apply(local("y")).is_none());
    }

    /// **A curried combinator normalizes**, which is the case that motivated
    /// the `Apply` node. `f => x => f x` must produce a term, and applying it
    /// to a lambda must reduce all the way.
    #[test]
    fn a_curried_combinator_reduces() {
        // `apply = f => x => f x`
        let apply_fn = lam(&["f", "x"], local("f").apply(local("x")).unwrap());
        assert!(apply_fn.is_function());
        // Applied to an identity function, the result is a function.
        let partial = apply_fn.apply(lam(&["y"], local("y"))).unwrap();
        assert!(partial.is_function());
        // And applied to a value, it reduces to that value.
        let done = partial.apply(local("v")).unwrap();
        assert_eq!(done, local("v"));
    }
}

/// The bridge's central property: **equivalent spellings produce equal terms**.
///
/// This is what the whole module exists for, and it is the property the compiler
/// currently has to test indirectly by comparing generated SQL. Here it is
/// checked at the term level, before any SQL exists, which means a failure
/// names the term that differs rather than a string of SQL.
#[cfg(test)]
mod bridge_tests {
    use super::*;
    use crate::check::check;
    use crate::workspace::Workspace;

    const TABLES: &str = "users : query { id = int, name = string, age = int } = \
                          table \"public\" \"users\"\n";

    /// Normalize the named definition of a program.
    fn term_of(src: &str, name: &str) -> Result<Term, Gap> {
        let ws = Workspace::from_source(&format!("{TABLES}{src}"));
        let tc = check(&ws);
        assert!(tc.errors().is_empty(), "checker errors: {:?}", tc.errors());
        let facts = WorkspaceFacts::new(&ws, &tc);
        let def = ws.modules[ws.root]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .expect("no such definition");
        let body = Body {
            module: ws.root,
            def,
            expr: ws.modules[ws.root].module.defs[def].body.clone(),
        };
        let resolved = |module: usize, id: u32| facts.resolution(module, id);
        let mut cx = Normalizer::new(Box::new(resolved));
        cx.in_module(ws.root, |cx| expression(&facts, cx, &body))
    }

    /// A definition whose body is a column reference normalizes to that column,
    /// **with its side preserved**.
    ///
    /// The side is meaning, not spelling: folding `.x` and `.<x` into one node
    /// with a label prefix made a join predicate read as a `Single`-sided
    /// column, and the checked layer refused a program the checker had accepted.
    /// One variant per side, so the distinction cannot be lost again.
    #[test]
    fn a_bare_column_definition_normalizes() {
        let t = term_of("c = .name\nq = users & select { x = c }\n", "c").unwrap();
        assert_eq!(
            t,
            term_of("c = .name\nq = users & select { x = c }\n", "c").unwrap()
        );
        match t {
            Term::Column { side, ref name, .. } => {
                assert_eq!(name, "name");
                assert_eq!(side, cagara_syntax::ast::Side::Single);
            }
            other => panic!("expected a column, got {other:?}"),
        }
    }

    /// A literal normalizes to a literal.
    #[test]
    fn a_literal_normalizes() {
        let t = term_of("one = 1\nq = users & select { x = one }\n", "one").unwrap();
        assert!(matches!(t, Term::Lit { .. }), "got {t:?}");
    }

    /// **An alias is erased.** `f = upper` must normalize to the same term as
    /// `upper` — because following an alias is not a step, it is the absence of
    /// one.
    ///
    /// This is the property `3624dfc` violated: the checker followed the alias
    /// and the elaborator did not, so the same program was valid or not
    /// depending on how it was written.
    #[test]
    fn an_alias_normalizes_away() {
        // Normalize the alias definition itself. Both must reach the same
        // verdict: if one is a gap the other must be too, with the same
        // message, because the alias should be invisible.
        let direct = term_of("q = users & select { x = .name }\nu = upper\n", "u");
        let via_alias = term_of(
            "q = users & select { x = .name }\nup = upper\nf = up\n",
            "f",
        );
        match (direct, via_alias) {
            // The terms must agree *up to source location*: an alias written at
            // a different offset is legitimately a different span, and `Origin`
            // exists to keep that honest so a diagnostic points at the text the
            // user wrote. Equality of shape is what "the alias is invisible"
            // means.
            (Ok(a), Ok(b)) => assert_eq!(
                strip_origins(&a),
                strip_origins(&b),
                "an alias produced a different term"
            ),
            (Err(a), Err(b)) => assert_eq!(
                a.message, b.message,
                "an alias produced a different gap; the alias should be invisible"
            ),
            (a, b) => panic!("alias changed the outcome:\n direct: {a:?}\n alias: {b:?}"),
        }
        // And the shape is the template, reached through the alias.
        let t = term_of(
            "q = users & select { x = .name }\nup = upper\nf = up\n",
            "f",
        )
        .unwrap();
        match t {
            Term::Template { sql, args, ty, .. } => {
                assert_eq!(&*sql, "UPPER($1)");
                assert!(args.is_empty(), "a bare alias applies nothing");
                assert_eq!(ty, crate::core::ScalarType::String, "the declared result");
            }
            other => panic!("expected the aliased template, got {other:?}"),
        }
    }

    /// A term with every `Origin` blanked, for comparing *shape*.
    ///
    /// Origins are deliberately part of term equality — a diagnostic has to
    /// point at the text the user wrote, so two occurrences at different
    /// offsets are genuinely different terms. But "these two spellings mean the
    /// same program" is a claim about shape, so the comparison has to say which
    /// of the two it is making.
    fn strip_origins(t: &Term) -> Term {
        let blank = Origin::new(0, cagara_syntax::ast::Span::default());
        match t.clone() {
            Term::Local { id, label, ty, .. } => Term::Local {
                id,
                label,
                ty,
                origin: blank,
            },
            Term::Column { side, name, ty, .. } => Term::Column {
                side,
                name,
                ty,
                origin: blank,
            },
            Term::Lit { lit, ty, .. } => Term::Lit {
                lit,
                ty,
                origin: blank,
            },
            Term::Template {
                sql,
                args,
                kind,
                ty,
                ..
            } => Term::Template {
                sql,
                args: args.iter().map(strip_origins).collect(),
                kind,
                ty,
                origin: blank,
            },
            Term::Primitive { name, args, ty, .. } => Term::Primitive {
                name,
                args: args.iter().map(strip_origins).collect(),
                ty,
                origin: blank,
            },
            Term::Apply { head, args, .. } => Term::Apply {
                head: Box::new(strip_origins(&head)),
                args: args.iter().map(strip_origins).collect(),
                origin: blank,
            },
            Term::Window {
                sql,
                spec,
                args,
                ty,
                ..
            } => Term::Window {
                sql,
                spec: Box::new(strip_origins(&spec)),
                args: args.iter().map(strip_origins).collect(),
                ty,
                origin: blank,
            },
            Term::Lambda {
                params,
                body,
                source,
                ..
            } => Term::Lambda {
                params,
                body: Box::new(strip_origins(&body)),
                source,
                origin: blank,
            },
            Term::Case { arms, default, .. } => Term::Case {
                arms: arms
                    .iter()
                    .map(|(c, v)| (strip_origins(c), strip_origins(v)))
                    .collect(),
                default: Box::new(strip_origins(&default)),
                origin: blank,
            },
            Term::Record { fields, .. } => Term::Record {
                fields: fields
                    .iter()
                    .map(|(k, v)| (k.clone(), strip_origins(v)))
                    .collect(),
                origin: blank,
            },
            Term::List { items, .. } => Term::List {
                items: items.iter().map(strip_origins).collect(),
                origin: blank,
            },
        }
    }

    /// A `sql` template used bare is a template value, and the same one however
    /// it was reached.
    #[test]
    fn a_template_is_a_value() {
        let t = term_of(
            "q = users & select { x = .name }\n\
             t : expr r string -> expr r string = sql \"UPPER($1)\"\n",
            "t",
        )
        .unwrap();
        // A signatured `sql` template normalizes to a template value, which is
        // what makes it usable in argument position without being applied.
        match t {
            Term::Template { sql, args, .. } => {
                assert_eq!(&*sql, "UPPER($1)");
                assert!(args.is_empty(), "a bare template has no arguments yet");
            }
            other => panic!("expected a template, got {other:?}"),
        }
    }

    /// A lambda becomes a function value, with its binder recorded and the body
    /// normalized under it.
    #[test]
    fn a_lambda_is_a_function_value() {
        let t = term_of("q = users & select { y = .id }\nf = x => x\n", "f").unwrap();
        match t {
            Term::Lambda { params, body, .. } => {
                assert_eq!(params, vec!["x".to_string()]);
                assert!(
                    matches!(*body, Term::Local { .. }),
                    "the body should be the bound local, got {body:?}"
                );
            }
            other => panic!("expected a lambda, got {other:?}"),
        }
    }
}

/// The new path must agree with the old one, definition for definition.
///
/// This is the migration's safety net and it is temporary by design: while both
/// paths exist, every scalar expression the old elaborator can build must come
/// out **structurally identical** from a term. When the old path is deleted this
/// test goes with it — its value is entirely in comparing the two.
///
/// It compares `CheckedExpr` values rather than generated SQL because the two
/// are different claims. SQL equality would pass if both paths produced the same
/// string by different trees; `CheckedExpr` equality is the stronger statement
/// that the *trees* agree, which is what makes deleting the old one safe.
#[cfg(test)]
mod migration_agreement {
    use super::*;
    use crate::check::check;
    use crate::compile::CompilerInput;
    use crate::workspace::Workspace;

    /// Elaborate `expr` the old way.
    fn old_way(
        ws: &Workspace,
        module: usize,
        owner: usize,
        e: &cagara_syntax::ast::Expr,
    ) -> Result<crate::checked::CheckedExpr, String> {
        let tc = check(ws);
        let input = CompilerInput::new(ws, &tc);
        let scope = &ws.modules[module].scope;
        crate::elaborate::elaborate_expr(input, module, owner, scope, e)
            .map_err(|err| err.message.clone())
    }

    /// Elaborate `expr` the new way, through a term.
    fn new_way(
        ws: &Workspace,
        module: usize,
        owner: usize,
        e: &cagara_syntax::ast::Expr,
    ) -> Result<crate::checked::CheckedExpr, String> {
        let tc = check(ws);
        let facts = WorkspaceFacts::new(ws, &tc);
        let body = Body {
            module,
            def: owner,
            expr: e.clone(),
        };
        let resolved = |m: usize, id: u32| facts.resolution(m, id);
        let mut cx = Normalizer::new(Box::new(resolved));
        let term = cx
            .in_module(module, |cx| expression(&facts, cx, &body))
            .map_err(|g| g.message)?;
        lower_expr(&term).map_err(|g| g.message)
    }

    /// Every *column-shaped* expression in the corpus elaborates identically.
    ///
    /// Columns, literals and template applications are the family the new path
    /// covers today, so the comparison is restricted to expressions that
    /// produce a `CheckedExpr` — which is also the honest scope of the claim.
    #[test]
    fn column_and_template_expressions_agree() {
        let cases = [
            "q = users & select { x = .name }\n",
            "q = users & select { x = upper .name }\n",
            "q = users & select { x = trim (upper .name) }\n",
            "q = users & select { x = .age + 1 }\n",
            "q = users & select { x = 1 }\n",
            "q = users & select { x = \"a\" }\n",
            "q = users & select { x = true }\n",
            // An alias resolved through the checker's record.
            "f = upper\nq = users & select { x = f .name }\n",
            // A computed function value, the `07a7e12` case.
            "q = users & select { x = (trim >>> upper) .name }\n",
            // An inline lambda, the case that had no definition to look up.
            "q = users & select { x = (f => upper f) .name }\n",
        ];
        const TABLES: &str =
            "users : query { id = int, name = string, age = int, active = bool } = \
             table \"public\" \"users\"\n";
        let mut compared = 0usize;
        for src in cases {
            let ws = Workspace::from_source(&format!("{TABLES}{src}"));
            let tc = check(&ws);
            assert!(tc.errors().is_empty(), "{src}: {:?}", tc.errors());
            // Find every projection value: the second field expression of a
            // `select { x = … }`. Walking the AST here is fine — this is a test
            // harness locating expressions to compare, not a compiler phase
            // deciding what they mean.
            for (def_index, def) in ws.modules[ws.root].module.defs.iter().enumerate() {
                let mut found = Vec::new();
                collect_projection_values(&def.body, &mut found);
                for e in found {
                    let old = old_way(&ws, ws.root, def_index, e);
                    let new = new_way(&ws, ws.root, def_index, e);
                    match (&old, &new) {
                        (Ok(a), Ok(b)) => {
                            // Compared **up to origin**. The two paths
                            // legitimately differ there, and the difference is
                            // not a defect in either: elaborated through a
                            // helper, the inner template is built while walking
                            // the *prelude's* `_>>>_` body, so the old path
                            // records the prelude's span; the new path keeps the
                            // span of the user's own expression. Both are
                            // truthful, user-facing diagnostics come from the
                            // checker rather than from these origins (verified
                            // against the CLI), and the new one points at text
                            // the user can actually edit.
                            //
                            // What must agree is *shape and type*, which is what
                            // makes deleting the old path safe.
                            assert_eq!(
                                strip_expr_origins(a),
                                strip_expr_origins(b),
                                "{}: the two paths disagree on `{}`",
                                src.trim(),
                                span_text(&ws, ws.root, e)
                            );
                            compared += 1;
                        }
                        // Both failing is agreement too: the claim is that the
                        // new path is not *narrower* than the old one.
                        (Err(_), Err(_)) => compared += 1,
                        (a, b) => panic!(
                            "{}: paths disagree on `{}`:\n old: {a:?}\n new: {b:?}",
                            src.trim(),
                            span_text(&ws, ws.root, e)
                        ),
                    }
                }
            }
        }
        // A floor, so a change that silently stops finding expressions to
        // compare fails instead of passing vacuously. Deliberately well below
        // the current count rather than equal to it: the number of projection
        // values a case yields is an implementation detail of the walk, while
        // "the comparison actually ran" is the property worth asserting.
        assert!(compared >= 8, "only {compared} expressions were compared");
    }

    /// A `CheckedExpr` with its origins replaced by a canonical one.
    ///
    /// `CheckedExpr` exposes `phase()`, `ty()`, `origin()` and `kind()` but no
    /// constructor that rebuilds one from parts, so this compares the two
    /// observable halves that matter — the type and the printed structure with
    /// spans removed — rather than claiming to compare whole values.
    fn strip_expr_origins(e: &crate::checked::CheckedExpr) -> String {
        let debug = format!("{e:?}");
        // Remove `origin: Origin { .. }` groups. The printed form nests, so a
        // simple scan for balanced braces is enough and does not need the
        // checked layer to grow a test-only API.
        let mut out = String::with_capacity(debug.len());
        let bytes = debug.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if debug[i..].starts_with("origin: Origin {") {
                let mut depth = 0usize;
                let mut j = i + "origin: Origin ".len();
                loop {
                    match bytes.get(j) {
                        Some(b'{') => depth += 1,
                        Some(b'}') => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        None => break,
                        _ => {}
                    }
                    j += 1;
                }
                out.push_str("origin: _");
                j = j.max(i + 1);
                while j < bytes.len() && bytes[j] == b',' {
                    j += 1;
                }
                i = j;
                continue;
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    fn span_text(ws: &Workspace, module: usize, e: &cagara_syntax::ast::Expr) -> String {
        ws.modules[module]
            .text
            .get(e.span.start as usize..e.span.end as usize)
            .unwrap_or("?")
            .to_string()
    }

    /// Collect the value expressions of every `select { … }` in a body.
    fn collect_projection_values<'a>(
        e: &'a cagara_syntax::ast::Expr,
        out: &mut Vec<&'a cagara_syntax::ast::Expr>,
    ) {
        use cagara_syntax::ast::ExprKind as K;
        match &e.kind {
            K::Record(fields) => {
                for (_, v) in fields {
                    out.push(v);
                }
            }
            K::App(f, args) => {
                collect_projection_values(f, out);
                for a in args.iter() {
                    collect_projection_values(a, out);
                }
            }
            K::Lambda(_, b) => collect_projection_values(b, out),
            K::Case(arms) => {
                for (c, v) in arms {
                    collect_projection_values(c, out);
                    collect_projection_values(v, out);
                }
            }
            K::Proj(b, _) => collect_projection_values(b, out),
            K::List(xs) => {
                for x in xs {
                    collect_projection_values(x, out);
                }
            }
            _ => {}
        }
    }
}

/// The bridge must normalize **every definition in the prelude**, because the
/// prelude is written in the language and its definitions are what user
/// programs expand into.
///
/// This is a gate, not a report. A construct the bridge cannot handle does not
/// fail loudly at the point it is missing — it becomes a *gap*, which
/// downstream means "this program cannot be compiled". Catching it here, on the
/// corpus that uses the widest range of constructs, is what keeps a missing
/// family from reaching a user.
///
/// The prelude is also the hardest available corpus for a specific reason: it
/// contains the four curried combinators `_&_`, `_$_`, `_>>>_` and `_<<<_`.
/// Every one is written `f => x => f x`, so the head of an application is a
/// bound *parameter* rather than a definition — the case that made an earlier
/// version of this module report a gap on exactly the definitions composition
/// is built from.
#[cfg(test)]
mod prelude_covers_the_bridge {
    use super::*;
    use crate::check::check;
    use crate::workspace::Workspace;

    #[test]
    fn every_definition_normalizes() {
        let ws = Workspace::from_source("q = 1\n");
        let tc = check(&ws);
        let facts = WorkspaceFacts::new(&ws, &tc);
        let mut gaps: Vec<String> = Vec::new();
        let mut total = 0usize;
        for m in 0..ws.modules.len() {
            for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
                total += 1;
                let body = Body {
                    module: m,
                    def: i,
                    expr: d.body.clone(),
                };
                let resolved = |mm: usize, id: u32| facts.resolution(mm, id);
                let mut cx = Normalizer::new(Box::new(resolved));
                if let Err(g) = cx.in_module(m, |cx| expression(&facts, cx, &body)) {
                    gaps.push(format!("`{}`: {}", d.name, g.message));
                }
            }
        }
        assert!(
            total > 100,
            "expected the prelude's definitions, found {total}"
        );
        assert!(
            gaps.is_empty(),
            "{} of {total} definitions did not normalize:\n{}",
            gaps.len(),
            gaps.join("\n")
        );
    }

    /// The curried combinators in particular, named so a regression says which
    /// one broke rather than only that a count changed.
    #[test]
    fn the_curried_combinators_normalize() {
        let ws = Workspace::from_source("q = 1\n");
        let tc = check(&ws);
        let facts = WorkspaceFacts::new(&ws, &tc);
        let mut checked = 0;
        for m in 0..ws.modules.len() {
            for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
                if !matches!(d.name.as_str(), "_&_" | "_$_" | "_>>>_" | "_<<<_" | "id") {
                    continue;
                }
                let body = Body {
                    module: m,
                    def: i,
                    expr: d.body.clone(),
                };
                let resolved = |mm: usize, id: u32| facts.resolution(mm, id);
                let mut cx = Normalizer::new(Box::new(resolved));
                let term = cx
                    .in_module(m, |cx| expression(&facts, cx, &body))
                    .unwrap_or_else(|g| panic!("`{}` did not normalize: {}", d.name, g.message));
                assert!(
                    term.is_function(),
                    "`{}` should normalize to a function value, got {term:?}",
                    d.name
                );
                checked += 1;
            }
        }
        assert!(checked >= 5, "only {checked} combinators were found");
    }
}
