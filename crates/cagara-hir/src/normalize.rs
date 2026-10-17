//! Normalization: reduce a resolved term to a form that no longer depends on
//! how it was *spelled*.
//!
//! # What this replaced
//!
//! Elaboration used to double as the normalization step, and it did so by
//! re-walking **source**: applying a function built an environment and walked
//! the callee's body again, from the AST. That one decision was the root of a
//! family of workarounds, each of which existed only because there was no
//! separate normalization pass:
//!
//! * a bare alias has no body to re-walk, so `follow_alias` walked the chain by
//!   hand;
//! * a composed function value has no body either, so
//!   `apply_to_computed_head` had to be added for `(trim >>> upper) .name`;
//! * a callee was classified by its *syntax shape* (`elaborate_callable`),
//!   because there was no normalized function value to inspect;
//! * a polymorphic definition was re-elaborated once per hole assignment,
//!   because substitution baked the choice in at expansion time.
//!
//! **All four are gone.** This list is kept as the record of what the
//! normalization pass paid for, not as a description of the code: the source
//! path for scalar expressions went first, and the query path followed it, so
//! `follow_alias`, `elaborate_callable`, `apply_to_computed_head` and the
//! parallel window, frame and order helpers they had grown no longer exist.
//! `elaborate.rs` is 270 lines where it was 2329, and nothing in this file
//! classifies a callee, follows an alias, or re-walks a body.
//!
//! The exception that outlived all of it is the **window specification**. A
//! window's `sql` and arguments arrive as a term like any other, but its
//! `partition`/`order`/`frame` record is handed to [`lower_winspec`] rather than
//! walked inline — a hook, not a second reading, and the reason the query
//! lowering had to be given one. It is named here because it is the last place
//! where a term's *structure* is dispatched on rather than its rule applied.
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
//! `(f => upper f) .name` — normalize to the *same term*. That property used to
//! be assertable only by comparing generated SQL (`cagara-sql`'s
//! spelling-equivalence tests); under normalization it holds by construction, at
//! the term level, before any SQL exists. Those tests remain, as the end-to-end
//! statement of the same thing.
//!
//! # Scope
//!
//! This module **is** the production path for a scalar expression: every one is
//! normalized to a term and lowered through the checked constructors, and a form
//! with no rule is a compiler gap rather than a second implementation of the
//! language. The gate for that is [`crate::compile`]'s
//! `the_normalized_route_handles_every_construct`, which counts the gaps over
//! every program shape the suite exercises and asserts the count is zero.
//!
//! It covers expressions **and** relational construction. The comment here used
//! to say the query half was still elaborated by walking source, with
//! `follow_alias` and `elaborate_callable` needed for it; both are gone, and
//! [`crate::elaborate`]'s only job now is to decide whether a definition is a
//! query and hand it to this module. The gate's zero is therefore a statement
//! about the whole language rather than about one layer of it.
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

#[cfg(test)]
thread_local! {
    /// Every expression the bridge visited, in visit order, as
    /// `(module, def, ExprId)`.
    ///
    /// The bridge walks source syntax, and the checker's typed tree is its own
    /// account of the same body. This is how a test compares the two walks:
    /// they have to cover the same nodes before a phase can take its children
    /// from the tree instead of re-deriving them from syntax.
    static VISITED: std::cell::RefCell<Vec<(usize, usize, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The nodes the bridge has visited since the last [`clear_visited`].
#[cfg(test)]
pub(crate) fn visited_nodes() -> Vec<(usize, usize, u32)> {
    VISITED.with(|v| v.borrow().clone())
}

/// Forget the visits so far, so one test can attribute them to one program.
#[cfg(test)]
pub(crate) fn clear_visited() {
    VISITED.with(|v| v.borrow_mut().clear());
}

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
///
/// One enum with one reader ([`declared_template_kind`]) on purpose. Elaboration
/// carries the same three cases and is the reason both exist — it built the
/// checked node from a `kind` read off the same signature, from the same three
/// strings, at the same time this layer did. Two enums and two readers of one
/// rule is the duplication this migration set out to remove, and a `head` string
/// added to one and not the other would have been a silently different answer
/// rather than a compile error.
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
    ///
    /// `row` is the same kind of fact one level up: the columns the checker gave
    /// the occurrence, read from its recorded type. `table "s" "t"` is a
    /// primitive whose arguments are two strings, so the term says nothing about
    /// its columns, while the checked constructor refuses to build a table
    /// without them. The row is recorded here — where the occurrence's `ExprId`
    /// is still in hand — rather than recovered later from the *declaring
    /// definition's* signature, which is a different fact and the wrong one as
    /// soon as two tables are inlined into one program. `None` for every
    /// primitive that denotes no row, and for a node the checker did not close.
    Primitive {
        name: Arc<str>,
        args: Vec<Term>,
        ty: crate::core::ScalarType,
        row: Option<std::sync::Arc<Vec<(String, crate::core::ScalarType)>>>,
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
    /// (`lower_winspec` below) rather than by a second implementation here.
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
        /// The scalar type the checker gave the whole `case`.
        ///
        /// Carried rather than inferred from an arm: a `case`'s arms are all
        /// checked at one type, and reading that type off whichever arm happens
        /// to come first would be a second guess at a fact the checker already
        /// settled. Its absence made a `case` used as a `where` predicate fail
        /// with "must be bool, found ?".
        ty: crate::core::ScalarType,
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
                // A **window** template's first argument is its spec, not a
                // scalar, so once that argument arrives the node becomes a
                // `Window` — which is what lets the spec be lowered by the code
                // that owns its rules instead of by an expression lowering that
                // would have to reinvent them.
                if kind == TemplateKind::Win && args.len() == 1 {
                    let spec = args.pop().expect("just pushed");
                    return Some(Term::Window {
                        sql,
                        spec: Box::new(spec),
                        args: Vec::new(),
                        ty,
                        origin,
                    });
                }
                Some(Term::Template {
                    sql,
                    args,
                    kind,
                    ty,
                    origin,
                })
            }
            // A window application collecting its value arguments.
            Term::Window {
                sql,
                spec: spec_term,
                mut args,
                ty,
                origin,
            } => {
                args.push(arg);
                Some(Term::Window {
                    sql,
                    spec: spec_term,
                    args,
                    ty,
                    origin,
                })
            }
            // A primitive accumulates the same way.
            //
            Term::Primitive {
                name,
                mut args,
                ty,
                row,
                origin,
            } => {
                args.push(arg);
                Some(Term::Primitive {
                    name,
                    args,
                    ty,
                    row,
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
///
fn apply_type_and_span(
    term: Term,
    ty: crate::core::ScalarType,
    row: Option<Vec<(String, crate::core::ScalarType)>>,
    origin: Origin,
) -> Term {
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
            row: current_row,
            ..
        } => Term::Primitive {
            name,
            args,
            ty: prefer(&current),
            // The application's row when it has one, otherwise the one already
            // carried: a primitive that is not a table keeps `None`, and a
            // saturated table takes the row of the node that saturated it.
            row: row.map(Arc::new).or(current_row),
            origin,
        },
        other => other,
    }
}

/// Replace every free reference to `name` in `term` with `arg`.
///
/// Substitution is **capture-avoiding**, and it has to be. An earlier version
/// of this function asserted the opposite — that binders in the normal form are
/// unique, so an argument could never be captured by an inner binder sharing a
/// name. That invariant does not hold, because inlining puts the binders of two
/// definitions into one term and the *use site* of one can be a binder of the
/// other:
///
/// ```text
/// wrap = p => (q => p)      -- binds `q`
/// h    = q => wrap q        -- binds `q` too, and passes it in
/// main = h 1 2              -- must be 1, was 2
/// ```
///
/// `h 1` substitutes `1` for `h`'s `q`, but the argument's text sits under
/// `wrap`'s own `q`, which captured it. The fix is the standard one: a binder
/// that occurs free in the argument is renamed first, which restores the
/// uniqueness the old comment assumed. See [`rewrite_local`].
fn substitute(term: Term, name: &str, arg: &Term) -> Term {
    rewrite_local(term, name, &|_| arg.clone(), &free_locals(arg))
}

/// A binder name that no source file can spell.
///
/// The lexer's identifier rule is `[a-zA-Z_][a-zA-Z0-9_]*`, so `$` cannot occur
/// in a name the parser produced and a generated one can never collide with a
/// user's. The counter keeps two renames of the same source name apart, which
/// is what makes the renaming in [`rewrite_local`] safe when it happens twice
/// in one term.
fn fresh_binder(name: &str) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("{name}${}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The labels of the [`Term::Local`]s in `term` that no binder inside `term`
/// binds — the names a substitution of `term` would have to avoid capturing.
fn free_locals(term: &Term) -> Vec<String> {
    fn walk(term: &Term, bound: &mut Vec<String>, out: &mut Vec<String>) {
        match term {
            Term::Local { label, .. } => {
                if !bound.iter().any(|b| b == label) && !out.iter().any(|o| o == label) {
                    out.push(label.clone());
                }
            }
            Term::Lambda { params, body, .. } => {
                let depth = bound.len();
                bound.extend(params.iter().cloned());
                walk(body, bound, out);
                bound.truncate(depth);
            }
            Term::Template { args, .. } | Term::Primitive { args, .. } => {
                for a in args {
                    walk(a, bound, out);
                }
            }
            Term::Apply { head, args, .. } => {
                walk(head, bound, out);
                for a in args {
                    walk(a, bound, out);
                }
            }
            Term::Window { spec, args, .. } => {
                walk(spec, bound, out);
                for a in args {
                    walk(a, bound, out);
                }
            }
            Term::Case { arms, default, .. } => {
                for (c, v) in arms {
                    walk(c, bound, out);
                    walk(v, bound, out);
                }
                walk(default, bound, out);
            }
            Term::Record { fields, .. } => {
                for (_, v) in fields {
                    walk(v, bound, out);
                }
            }
            Term::List { items, .. } => {
                for i in items {
                    walk(i, bound, out);
                }
            }
            Term::Column { .. } | Term::Lit { .. } => {}
        }
    }
    let mut out = Vec::new();
    walk(term, &mut Vec::new(), &mut out);
    out
}

/// Rename one binder's references in `term`, stopping where an inner binder of
/// the same name shadows it — those references belong to the inner binder.
fn rename_binder(term: Term, from: &str, to: &str) -> Term {
    rewrite_local(
        term,
        from,
        &|local| match local {
            Term::Local { id, ty, origin, .. } => Term::Local {
                id,
                label: to.to_string(),
                ty,
                origin,
            },
            other => other,
        },
        &[],
    )
}

/// Replace every free reference to `name` in `term` with `replace` applied to
/// that reference.
///
/// `avoid` is the set of names the replacement introduces: a lambda parameter
/// that is one of them would capture the replacement, so it is renamed out of
/// the way before the walk descends. Renaming is conservative — only parameters
/// actually named in `avoid` are touched — so a substitution that cannot
/// capture is still a plain copy.
fn rewrite_local(term: Term, name: &str, replace: &dyn Fn(Term) -> Term, avoid: &[String]) -> Term {
    match term {
        // A local reference is replaced when it is the binder. Matching on the
        // *label* here is safe because resolution already happened — this is the
        // binder's own name — and because `avoid` has just removed every binder
        // that could capture the replacement.
        Term::Local { ref label, .. } if label == name => replace(term),
        Term::Local { .. } | Term::Column { .. } | Term::Lit { .. } => term,
        Term::Template {
            sql,
            args,
            kind,
            ty,
            origin,
        } => Term::Template {
            sql,
            args: args
                .into_iter()
                .map(|a| rewrite_local(a, name, replace, avoid))
                .collect(),
            kind,
            ty,
            origin,
        },
        Term::Primitive {
            name: pname,
            args,
            ty,
            row,
            origin,
        } => Term::Primitive {
            name: pname,
            args: args
                .into_iter()
                .map(|a| rewrite_local(a, name, replace, avoid))
                .collect(),
            ty,
            row,
            origin,
        },
        Term::Lambda {
            params,
            body,
            source,
            origin,
        } => {
            // Shadowing: an inner binder of the same name stops the
            // substitution, exactly as lexical scope requires.
            if params.iter().any(|p| p == name) {
                Term::Lambda {
                    params,
                    body,
                    source,
                    origin,
                }
            } else {
                // Capture-avoidance: a parameter the replacement mentions free
                // would capture it. Rename those binders — and their references,
                // and only theirs — before substituting under them.
                let mut renamed = Vec::with_capacity(params.len());
                let mut pairs = Vec::new();
                for p in params {
                    if avoid.iter().any(|a| a == &p) {
                        let fresh = fresh_binder(&p);
                        pairs.push((p, fresh.clone()));
                        renamed.push(fresh);
                    } else {
                        renamed.push(p);
                    }
                }
                let mut body = *body;
                for (from, to) in &pairs {
                    body = rename_binder(body, from, to);
                }
                Term::Lambda {
                    params: renamed,
                    body: Box::new(rewrite_local(body, name, replace, avoid)),
                    source,
                    origin,
                }
            }
        }
        // An unreduced application: substitute through both the head and the
        // arguments, then retry the reduction. Retrying matters — substituting
        // for the head is exactly what makes `f x` reduce when `f` arrives.
        Term::Apply { head, args, origin } => {
            let head = rewrite_local(*head, name, replace, avoid);
            let args: Vec<Term> = args
                .into_iter()
                .map(|a| rewrite_local(a, name, replace, avoid))
                .collect();
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
            spec: Box::new(rewrite_local(*spec, name, replace, avoid)),
            args: args
                .into_iter()
                .map(|a| rewrite_local(a, name, replace, avoid))
                .collect(),
            ty,
            origin,
        },
        Term::Case {
            arms,
            default,
            ty,
            origin,
        } => Term::Case {
            arms: arms
                .into_iter()
                .map(|(c, v)| {
                    (
                        rewrite_local(c, name, replace, avoid),
                        rewrite_local(v, name, replace, avoid),
                    )
                })
                .collect(),
            default: Box::new(rewrite_local(*default, name, replace, avoid)),
            ty,
            origin,
        },
        Term::Record { fields, origin } => Term::Record {
            fields: fields
                .into_iter()
                .map(|(k, v)| (k, rewrite_local(v, name, replace, avoid)))
                .collect(),
            origin,
        },
        Term::List { items, origin } => Term::List {
            items: items
                .into_iter()
                .map(|i| rewrite_local(i, name, replace, avoid))
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
    /// Bodys already normalized, so a diamond-shaped dependency is reduced once.
    cache: HashMap<DefId, Arc<Term>>,
    /// Bindings introduced by enclosing lambdas, innermost last.
    binders: Vec<String>,
    /// The module whose body is being walked, so a name is resolved against the
    /// records of the module it was *written in* rather than the caller's.
    current_module: usize,
    /// The overload candidates assigned to the open holes of each definition
    /// currently being walked, outermost first.
    ///
    /// A stack because the walk nests and a hole index is **local to the
    /// definition that owns it**.
    hole_stack: Vec<Vec<(usize, usize)>>,
    /// The definitions currently being inlined, outermost first.
    ///
    /// The term layer inlines rather than calls, so a definition that names
    /// itself would expand without end; this is the record of what is already
    /// being expanded. It used to mirror an equivalent stack in the elaborator,
    /// which no longer exists.
    active: Vec<DefId>,
    /// Whether the inline depth limit was hit, so the caller reports
    /// "too deep" rather than the cycle message `inlining` also returns `None`
    /// for. Two reasons for one `None` would otherwise be indistinguishable.
    too_deep: bool,
}

/// How many definitions may be inlined on top of one another before the walk
/// gives up.
///
/// A guard on the shape of the *input*, not on the shape of the answer: a chain
/// of definitions that merely name each other, or a few hundred chained
/// `where` stages, recurses once per link in this pass and in the SQL lowering
/// that follows it. Measured: 300 chained stages compiled, 600 overflowed the
/// stack and aborted the process with no diagnostic at all. The limit is set
/// well above any query written by hand — a 200-link chain is deep enough to
/// catch the accident while leaving real programs untouched — and hitting it is
/// reported, never silent.
pub const MAX_INLINE_DEPTH: usize = 200;

/// How many nodes one definition's normal form may hold.
///
/// A guard on *expansion*, not on the source: inlining is substitution, so a
/// definition that mentions its argument more than once multiplies the work, and
/// nesting those multiplies it again. Measured: `dup = x => ifThenElse true x x`
/// nested eighteen deep expands to about a quarter of a million nodes, which is
/// eight megabytes of SQL and ten seconds of work from 350 bytes of source; the
/// next two levels would be a minute and thirty-two megabytes. The bound is far
/// above any query written by hand — a large one is thousands of nodes — and
/// hitting it is reported, never silent.
///
/// It is checked on the finished term, so it bounds the lowering and the SQL
/// rather than the expansion that produced them; see the note at the check.
pub const MAX_TERM_NODES: usize = 200_000;

impl<'a> Normalizer<'a> {
    pub fn new(resolver: Box<Resolver<'a>>) -> Self {
        Self {
            resolver,
            cache: HashMap::new(),
            binders: Vec::new(),
            current_module: 0,
            hole_stack: Vec::new(),
            active: Vec::new(),
            too_deep: false,
        }
    }

    /// Whether the inline depth limit has been hit during this walk.
    ///
    /// Read after the walk: the term built under a definition that hit the
    /// limit is incomplete, so the caller discards it and reports the fault.
    pub fn hit_depth_limit(&self) -> bool {
        self.too_deep
    }

    /// The candidate assigned to hole `k` of the definition being walked.
    ///
    /// **Positional**, because that is what the assignment is: `cx.holes` in
    /// `elaborate.rs` is indexed by hole, `holes[k] = (callee_module,
    /// chosen_def)`, and the source path reads it as `cx.holes.get(h)`. An
    /// earlier version of this searched by the tuple's first element, which is
    /// the *module* — so it compared a module index against a hole index and
    /// found nothing, silently, for as long as no module happened to be numbered
    /// like a hole.
    ///
    /// Innermost assignment first, for the reason the stack exists: a hole index
    /// is local to its owner, so an outer definition's answer would be for a
    /// different hole carrying the same number.
    pub fn hole(&self, k: usize) -> Option<usize> {
        self.hole_stack
            .iter()
            .rev()
            .find_map(|assignment| assignment.get(k).map(|(_, chosen)| *chosen))
    }

    /// Walk with one definition's hole assignment in scope.
    pub fn with_holes<T>(
        &mut self,
        holes: Vec<(usize, usize)>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.hole_stack.push(holes);
        let out = f(self);
        self.hole_stack.pop();
        out
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

    /// Inline a definition's body, recording that it is being walked.
    ///
    /// The term layer **inlines** a definition rather than calling it, so a
    /// definition that names itself — `f = f`, or a cycle through two — inlines
    /// forever. The `active` stack is the only guard, here or anywhere: the
    /// source walk that used to apply the same rule is gone, so this is the
    /// single place where recursion is detected.
    ///
    /// Measured: without this, `f = f` overflows the stack rather than reporting
    /// "refers to itself".
    ///
    /// `None` means a cycle, and the caller reports it — the same verdict
    /// `enter` produces, so both routes agree on what recursion is.
    ///
    /// A definition's body is **closed**: the only binders in scope inside it
    /// are the lambdas written in it, so the caller's binders are set aside for
    /// the duration. Leaving them in place made a top-level name that happened
    /// to match an enclosing lambda parameter read as that parameter — `n = 5`,
    /// `g = n + 1`, `f = n => g`, `f 1` normalized `n` to the argument and
    /// emitted `1 + 1` where the answer is `6`.
    ///
    /// Inlining is also **bounded**. `active` catches a cycle, but a long *chain*
    /// of distinct definitions is not a cycle and still recurses once per link:
    /// a few hundred chained `where` stages — or an alias chain, where each
    /// definition simply names the next — exhausted the stack and aborted the
    /// process. A depth limit turns that into a diagnostic the user can act on.
    /// The bound is deliberately generous; it is a stack guard, not a style rule.
    pub fn inlining<T>(&mut self, id: DefId, f: impl FnOnce(&mut Self) -> T) -> Option<T> {
        if self.active.contains(&id) {
            return None;
        }
        // `active` holds the whole inline stack, so its length *is* the depth.
        // The nested check is what keeps a term that needs one more frame from
        // being built at all, rather than being reported after the fact.
        if self.active.len() >= MAX_INLINE_DEPTH {
            self.too_deep = true;
            return None;
        }
        self.active.push(id);
        let outer = std::mem::take(&mut self.binders);
        let out = f(self);
        self.binders = outer;
        self.active.pop();
        Some(out)
    }

    /// The normal form of a definition, reduced once and shared.
    pub fn define(&mut self, id: DefId, build: impl FnOnce(&mut Self) -> Term) -> Arc<Term> {
        if let Some(cached) = self.cache.get(&id) {
            return Arc::clone(cached);
        }
        let term = build(self);
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
    /// Borrowed from the adapter, not from a temporary: both adapters hold the
    /// workspace or the compiler input for the whole walk, so the borrow lives
    /// as long as the walk needs it and the bridge never copies a body.
    /// `None` when the definition does not exist in that module.
    fn def_body(&self, module: usize, def: usize) -> Option<Body<'_>>;

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

    /// The overload candidates the **caller** chose, keyed by hole index.
    ///
    /// `owner` is the definition whose body the use sits in — *not* the callee.
    /// `infer.rs` records a choice with `self.active.last()`, which is the
    /// definition being elaborated, so asking about the callee returns nothing.
    /// `site` is the callee *name* node's `ExprId`, which is where a definition
    /// is instantiated.
    ///
    /// The default answers "nothing recorded", which is the truthful answer for
    /// an adapter that has no checker to consult. An adapter that *has* one must
    /// override it with [`resolved_assignment`], because a default that silently
    /// answers "no assignment" makes every hole open and lets the fall-through
    /// path stand in for the assignment.
    fn holes_at(&self, owner: (usize, usize), site: u32) -> Vec<(usize, usize)> {
        let _ = (owner, site);
        Vec::new()
    }

    /// An already-elaborated binding from the *elaboration environment*, if the
    /// name occurrence is bound by an enclosing application rather than the
    /// module.
    /// The scalar type the checker gave the expression with this `ExprId`.
    ///
    /// `ScalarType::Unknown` when the checker recorded none, matching
    /// `TypeCheck::use_ty`: "the checker did not solve this" is a fact worth
    /// carrying rather than a reason to fail, because the definition was
    /// already reported.
    fn use_ty(&self, module: usize, id: u32) -> crate::core::ScalarType;

    /// The **row** the checker recorded for the expression with this `ExprId`.
    ///
    /// The structured counterpart of [`Facts::use_ty`], which answers with a
    /// scalar and therefore answers `None` for anything query-shaped. A
    /// `table "s" "t"` node is recorded as `query { id = int, name = string }`,
    /// and that row is what the checked table constructor needs; recovering it
    /// from the declaring definition's signature instead would be a *different*
    /// fact, and the wrong one once two tables are inlined into one program.
    ///
    /// `None` when the node got no row: not a query, or the checker did not
    /// close it.
    fn use_row(&self, module: usize, id: u32) -> Option<Vec<(String, crate::core::ScalarType)>> {
        let _ = (module, id);
        None
    }
}

/// The hole assignment a use site resolved, as `holes[k] = (callee_module,
/// The overload choice the checker recorded at one use site.
///
/// Shared by both [`Facts`] adapters, which had this logic twice — the same
/// duplication [`resolved_assignment`] was extracted to remove, and for the same
/// reason: two copies of a rule are two chances to disagree, and the adapters
/// are supposed to be interchangeable.
///
/// **A use site has exactly one hole in every program measured.** The checker
/// keys choices by `(ExprId, hole)`, and each overloaded *application* is its
/// own `ExprId`, so `1 + 2 + 3 + 4`, `f (1 + 2) (3 + 4)` and a record of two
/// sums each record one entry per site with `hole = 0`. The multi-entry branch
/// is therefore defensive against a state nothing produces today.
///
/// It stays, and stays a `None` rather than "take the first", because the
/// alternative is guessing: with several entries at one site there is no way to
/// tell from the site alone *which* hole the caller meant, and `choices_of` is
/// sorted by source order rather than by relevance. Reporting it as unresolved
/// makes a future multi-hole site a visible gap instead of a silent coin-flip;
/// threading the hole index through the trait would be the right fix at that
/// point, not before it.
fn choice_at(
    tc: &crate::check::TypeCheck,
    module: usize,
    owner: usize,
    site: u32,
) -> Option<Choice> {
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

/// chosen_def)` — the canonical answer to [`Facts::holes_at`].
///
/// Shared by both adapters so the production path and the tests cannot answer
/// this question differently. They did: `impl Facts for WorkspaceFacts` never
/// overrode the method, so every test elaborated with *no* assignment on the
/// stack, leaving the fall-through path — an enclosing assignment, or nothing —
/// to stand in for the assignment the checker had actually resolved. Tests then
/// covered a route production never takes.
///
/// **Kept in the checker's order, with open holes dropped**, which is what makes
/// the *position* of an entry equal to its hole index — the read
/// [`Normalizer::hole`] documents. Dropping an entry shifts every later
/// position, so that equality holds only while no site mixes a resolved choice
/// with an open one;
/// `compile::tests::every_use_site_records_contiguous_choices` states that
/// invariant, because nothing in the checker does.
///
/// An open hole is left out deliberately: it is answered by the *enclosing*
/// assignment sitting on the normalizer's stack, not by this one.
fn resolved_assignment(
    tc: &crate::check::TypeCheck,
    module: usize,
    owner: usize,
    site: u32,
) -> Vec<(usize, usize)> {
    tc.choices_of(module, owner)
        .into_iter()
        .filter(|(recorded, _, _)| *recorded == site)
        .filter_map(|(_, hole, choice)| match choice {
            Choice::Def(_, chosen) => Some((hole, chosen)),
            Choice::Hole(_) => None,
        })
        .collect()
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
///
/// The expression is **borrowed**, not owned. The walk recurses into
/// sub-expressions, and an owned body had to clone the child it stepped into —
/// and an `ast::Expr` owns its children, so cloning a node cloned the whole
/// subtree under it. A pipeline nested *n* deep was copied O(n²) times before a
/// single term existed. Borrowing is also the shape a walk needs to carry
/// anything else the checker recorded about the same body, since that is
/// borrowed from the `TypeCheck` the walk already holds.
#[derive(Debug, Clone, Copy)]
pub struct Body<'a> {
    pub module: usize,
    pub def: usize,
    pub expr: &'a cagara_syntax::ast::Expr,
}

impl<'a> Body<'a> {
    /// A body at one of its sub-expressions, keeping the module and definition.
    ///
    /// The child's lifetime is the parent's: every sub-expression of a body
    /// lives inside that body.
    pub fn child(&self, expr: &'a cagara_syntax::ast::Expr) -> Body<'a> {
        Body {
            module: self.module,
            def: self.def,
            expr,
        }
    }
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
    /// Whether the *program* is wrong rather than this phase being unable to
    /// build it.
    ///
    /// The distinction is the one `Fault` draws for the checked layer, and it
    /// matters at the same boundary: a `Gap` means "normalization has no rule
    /// for this", which sends a caller down its fallback route, while a rule the
    /// program *broke* — `limit` given something that is not a count — is the
    /// program's to fix and must be reported rather than routed around. Most
    /// gaps are capability gaps, so that is the default and the program-error
    /// case opts in.
    pub fault: crate::core::Fault,
}

impl Gap {
    pub fn new(origin: Origin, message: impl Into<String>) -> Self {
        Self {
            origin,
            message: message.into(),
            fault: crate::core::Fault::Compiler,
        }
    }

    /// A gap the **program** caused: a rule it broke, not a rule that is
    /// missing. Reported to the user rather than falling back.
    pub fn program(origin: Origin, message: impl Into<String>) -> Self {
        Self {
            origin,
            message: message.into(),
            fault: crate::core::Fault::Program,
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
    #[cfg(test)]
    VISITED.with(|v| v.borrow_mut().push((module, body.def, e.id)));

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
        ExprKind::Name(n) => inline_resolved(facts, cx, module, body.def, e.id, n, origin),
        // `alias.name`: a definition reached through an imported module.
        //
        // The checker records the *projection's* resolution exactly as it
        // records a bare name's — `infer_proj` writes the field's binding at the
        // projection node's own `ExprId` — so resolving and inlining are the
        // same steps here rather than a second set of rules for the qualified
        // spelling. That record is what this arm was missing: without it the
        // form had no rule at all and was reported as a compiler gap.
        //
        // A module alias shadowed by an enclosing binder is not a module; the
        // checker refuses that use, so it is reported rather than resolved
        // through the top-level alias that happens to share the name.
        ExprKind::Proj(base, f) => {
            if let ExprKind::Name(alias) = &base.kind {
                if cx.is_local(alias) {
                    return Err(Gap::new(
                        origin,
                        format!("`{alias}` is a local binding here, not a module"),
                    ));
                }
            }
            inline_resolved(facts, cx, module, body.def, e.id, f, origin)
        }
        // A lambda: record the binders, normalize the body under them.
        ExprKind::Lambda(param, body_expr) => {
            let inner =
                cx.with_binder(param, |cx| expression(facts, cx, &body.child(body_expr)))?;
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
            // The callee's open overloads are resolved by **this** application,
            // recorded against *this* definition at the id of the callee name
            // node. The id has to be read here, while the head is still a
            // `Name`: flattening an application discards that node, and a walk
            // that has lost it cannot find the assignment however well it looks.
            let pushed = match &head.kind {
                // A qualified callee (`lib.bind`) is the same question asked
                // through an import: the checker records its choices at the
                // projection node's own `ExprId`, exactly as it does for a bare
                // name, so the assignment is read from the same place. Leaving
                // the qualified spelling out is what made `lib.bind .id 1` an
                // unanswerable hole — the callee inlined, but with nothing on
                // the stack to answer its open overload.
                ExprKind::Name(_) | ExprKind::Proj(..) => {
                    let h = facts.holes_at((module, body.def), head.id);
                    Some(h)
                }
                _ => None,
            };
            let apply = |cx: &mut Normalizer<'_>| -> Result<Term, Gap> {
                let head_term = expression(facts, cx, &body.child(head))?;
                let mut arg_terms = Vec::with_capacity(args.len());
                for arg in args.iter() {
                    arg_terms.push(expression(facts, cx, &body.child(arg))?);
                }
                // Reduce as far as the head allows. An application that cannot
                // reduce is *kept*, not rejected: `f x` inside `f => f x`
                // has nothing to substitute into until the enclosing lambda is
                // applied, and every curried combinator in the prelude is
                // written that way.
                let reduced = reduce_application(head_term, arg_terms, origin);
                // A *saturated* template takes the application's own type and
                // span: reading the type off the head records the template's
                // result at the *definition's* `ExprId` while the elaborator and
                // the incremental facts read the type of the *use*, and the
                // head's span would blame `upper` rather than `upper .name`.
                //
                // The same argument applies to the **row**, and this is the only
                // place it can be picked up: `table "s" "t"` is a prelude
                // primitive applied here, so the saturated term is built while
                // this node is in hand, while the primitive's own definition in
                // the prelude has no query row at all. Reading it from the
                // *declaring definition* instead would be a different fact — and
                // the wrong one as soon as two tables are inlined into one
                // program, since each needs its own columns.
                Ok(apply_type_and_span(
                    reduced,
                    facts.use_ty(module, e.id),
                    facts.use_row(module, e.id),
                    origin,
                ))
            };
            match pushed {
                Some(holes) => cx.with_holes(holes, apply),
                None => apply(cx),
            }
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
                    expression(facts, cx, &body.child(c))?,
                    expression(facts, cx, &body.child(v))?,
                ));
            }
            let default = expression(facts, cx, &body.child(default))?;
            Ok(Term::Case {
                arms: out,
                default: Box::new(default),
                ty: facts.use_ty(module, e.id),
                origin,
            })
        }
        // A list literal: `inList [..] .x`. Kept in source order; the checked
        // layer takes the elements positionally.
        ExprKind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(expression(facts, cx, &body.child(item))?);
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
                out.push((name.clone(), expression(facts, cx, &body.child(value))?));
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
        //
        // The occurrence's *row* is read here, where its `ExprId` still is.
        // `table "s" "t"` carries no columns in its arguments, and the checked
        // constructor will not build a table without them, so the fact has to be
        // picked up while the node is in hand — a later phase has only a span,
        // and a span is not a node identity.
        ExprKind::Primitive(name) => Ok(Term::Primitive {
            name: std::sync::Arc::from(name.as_str()),
            args: Vec::new(),
            ty: facts.use_ty(module, e.id),
            row: facts.use_row(module, e.id).map(std::sync::Arc::new),
            origin,
        }),
        // A form normalization has no rule for. Reported as a gap rather than
        // guessed at, so a newly added expression family shows up as a named
        // compiler gap instead of a wrong term — and the corpus's
        // `no_corpus_program_hits_a_compiler_gap` fails on it.
        other => Err(Gap::new(
            origin,
            format!(
                "normalization has no rule for {}",
                describe_expr_kind(other)
            ),
        )),
    }
}

/// Inline what one resolved name occurrence denotes.
///
/// Shared by the bare-name arm and the qualified (`alias.name`) arm, because the
/// checker records the same kind of resolution for both and the rules that turn
/// one into a term are the same rules: a `sql` template is a value in its own
/// right, an overloaded name uses the candidate the checker picked **for this
/// use**, a definition is inlined under its own module, and a definition that
/// names itself is reported as recursion. A second copy for the qualified
/// spelling is exactly the duplication this module exists to remove.
///
/// `display` is the name as the program wrote it, for messages. `id` is the
/// occurrence's `ExprId`, which is what the resolution and the overload choice
/// are keyed by; for a qualified name it is the *field* (`shout` in
/// `lib.shout`), because that is the node the checker recorded facts for.
fn inline_resolved(
    facts: &dyn Facts,
    cx: &mut Normalizer<'_>,
    module: usize,
    owner: usize,
    id: u32,
    display: &str,
    origin: Origin,
) -> Result<Term, Gap> {
    match cx.resolve(module, id) {
        Some(Resolved::Def((dm, dd))) => {
            // A `sql` template is a value in its own right: inlining it
            // means building the template, not forcing it to a scalar.
            if let Some(t) = facts.def_template(dm, dd) {
                return Ok(Term::Template {
                    sql: std::sync::Arc::from(t.sql.as_str()),
                    args: Vec::new(),
                    kind: t.kind,
                    // The template's **declared result**, not this node's type.
                    // A bare name referring to a template has the template's
                    // *function* type at its use site (`expr r string ->
                    // expr r string`), and reading that would record a function
                    // where the applied result — `string` — belongs. The same
                    // rule as the overload path below, for the same reason.
                    ty: facts
                        .def_result_ty(dm, dd)
                        .unwrap_or(crate::core::ScalarType::Unknown),
                    origin,
                });
            }
            let inner = facts
                .def_body(dm, dd)
                .ok_or_else(|| Gap::new(origin, format!("`{display}` has no body to normalize")))?;
            // A definition that names itself would inline forever, so the walk
            // carries what it is already expanding and reports the cycle.
            // The callee's names were recorded under the **callee's** module, so
            // the walk moves there. This is what makes a user definition that
            // calls a prelude helper resolve the helper's own names correctly.
            cx.in_module(dm, |cx| {
                cx.inlining((dm, dd), |cx| expression(facts, cx, &inner))
                    .unwrap_or_else(|| Err(inline_limit_gap(cx, origin, display)))
            })
        }
        // An overloaded name: the checker picked one candidate **for this use**,
        // and that choice is recorded per use site rather than being a property
        // of the name. Consulting it is what makes `+` resolve to the int or the
        // float definition at each occurrence.
        Some(Resolved::Overloads { module: om, defs }) => {
            let chosen = match facts.choice(module, owner, id) {
                Some(Choice::Def(_, di)) => di,
                // The use sits at an open hole of the definition being
                // elaborated, so the answer is not recorded at this module and
                // site — it belongs to whoever applied the definition, and
                // travels in the assignment the walk pushed on entry.
                Some(Choice::Hole(k)) => match cx.hole(k) {
                    Some(chosen) => chosen,
                    None => {
                        return Err(Gap::new(
                            origin,
                            format!(
                                "`{display}` is an open overload (hole {k}) and no assignment \
                                 for it was supplied"
                            ),
                        ))
                    }
                },
                // Nothing recorded: the use is open inside its own definition.
                // Reported rather than defaulted, because taking candidate zero
                // would silently pick one of several valid readings.
                None => {
                    return Err(Gap::new(
                        origin,
                        format!("the checker recorded no candidate for the overloaded `{display}`"),
                    ))
                }
            };
            // A candidate that is not among the recorded ones is a contradiction
            // between two checker facts; reporting it is better than elaborating
            // a definition the use did not choose.
            if !defs.contains(&chosen) {
                return Err(Gap::new(
                    origin,
                    format!(
                        "`{display}` resolved to a definition that is not one of its candidates"
                    ),
                ));
            }
            if let Some(t) = facts.def_template(om, chosen) {
                return Ok(Term::Template {
                    sql: std::sync::Arc::from(t.sql.as_str()),
                    args: Vec::new(),
                    kind: t.kind,
                    // The **candidate's declared result type**, not this use
                    // site's. An overloaded operator is instantiated fresh per
                    // use, so the use's own type may still hold an unsolved
                    // variable — measured on `.age + 1`, whose application type
                    // is `expr r a` with `a` never solved — while the signature
                    // of the candidate the checker chose says plainly that it
                    // returns an int.
                    ty: facts
                        .def_result_ty(om, chosen)
                        .unwrap_or(crate::core::ScalarType::Unknown),
                    origin,
                });
            }
            let inner = facts.def_body(om, chosen).ok_or_else(|| {
                Gap::new(
                    origin,
                    format!("the chosen candidate of `{display}` has no body"),
                )
            })?;
            cx.in_module(om, |cx| {
                cx.inlining((om, chosen), |cx| expression(facts, cx, &inner))
                    .unwrap_or_else(|| Err(inline_limit_gap(cx, origin, display)))
            })
        }
        Some(Resolved::Primitive) => Ok(Term::Primitive {
            name: std::sync::Arc::from(display),
            args: Vec::new(),
            ty: facts.use_ty(module, id),
            row: facts.use_row(module, id).map(std::sync::Arc::new),
            origin,
        }),
        None => Err(Gap::new(
            origin,
            format!("the checker recorded no resolution for `{display}`"),
        )),
    }
}

/// Normalize and lower a whole **query** definition through the term layer.
///
/// The relational counterpart of [`elaborate_with_holes`]: a definition whose
/// declared scheme is a query is normalized to a term and lowered, instead of
/// being re-walked from source. Normalization resolves names, inlines
/// definitions and reduces applications — a stage's included — so the lowering
/// sees a saturated tree and has nothing left to look up.
///
/// `Err` with `Fault::Compiler` means the term layer could not build the
/// definition, which is the caller's cue to record the fallback.
#[cfg(test)]
pub fn elaborate_query_via_terms(
    input: crate::compile::CompilerInput<'_>,
    module: usize,
    owner: usize,
    e: &cagara_syntax::ast::Expr,
) -> Result<crate::checked::CheckedQuery, crate::core::Error> {
    let facts = InputFacts::new(input);
    let body = Body {
        module,
        def: owner,
        expr: e,
    };
    let resolve = |module: usize, id: u32| facts.resolution(module, id);
    let mut cx = Normalizer::new(Box::new(resolve));
    let to_error = |g: Gap| {
        let e = match g.fault {
            crate::core::Fault::Compiler => crate::core::Error::unsupported(g.message),
            _ => crate::core::Error::new(g.message),
        };
        e.at(g.origin)
    };
    let term = cx
        .in_module(module, |cx| expression(&facts, cx, &body))
        .map_err(to_error)?;
    // Inlining substitutes a definition's body for each of its uses, so a
    // definition that mentions its argument twice doubles it, and nesting those
    // doubles it again: `dup = x => ifThenElse true x x` nested eighteen deep
    // builds a term of a quarter of a million nodes from about 350 bytes of
    // source, and ten seconds and eight megabytes of SQL come out.
    //
    // The budget is a *program* fault — the program's shape is what expands.
    // It bounds what is **emitted**, and it catches a moderate expansion
    // immediately; it does not bound the expansion itself, which happens while
    // the term is built and before this runs. A deeply nested doubling chain
    // therefore still spends its time and is refused at the end of it. Bounding
    // the construction means charging the budget inside the reduction —
    // `substitute` is where the copying happens — which needs a failure channel
    // through `Term::apply`; see `docs/PLAN.md`.
    check_term_size(&term).map_err(to_error)?;
    // The normalized adapter is intentionally self-contained: it lowers the
    // normalized term through the checked constructors. Typed lowering owns
    // the checker-stage identity and rows on the production route; this
    // adapter is used only when that route declines a shape.
    lower_query(&term).map_err(to_error)
}

/// Refuse a normal form larger than [`MAX_TERM_NODES`].
///
/// Counted with an explicit stack rather than recursively: the terms this
/// rejects are exactly the deep and wide ones, and a walk that recursed over
/// them would overflow the stack while reporting that the term is too big.
#[cfg(test)]
fn check_term_size(t: &Term) -> Result<(), Gap> {
    let mut stack = vec![t];
    let mut nodes = 0usize;
    while let Some(t) = stack.pop() {
        nodes += 1;
        if nodes > MAX_TERM_NODES {
            return Err(Gap::program(
                other_origin(t),
                format!(
                    "this definition expands to more than {MAX_TERM_NODES} operations; \
                     a definition that uses its argument more than once doubles the work \
                     at every level"
                ),
            ));
        }
        match t {
            Term::Lambda { body, .. } => stack.push(body),
            Term::Apply { head, args, .. } => {
                stack.push(head);
                stack.extend(args);
            }
            Term::Template { args, .. }
            | Term::Primitive { args, .. }
            | Term::List { items: args, .. } => stack.extend(args),
            Term::Window { spec, args, .. } => {
                stack.push(spec);
                stack.extend(args);
            }
            Term::Case { arms, default, .. } => {
                for (c, v) in arms {
                    stack.push(c);
                    stack.push(v);
                }
                stack.push(default);
            }
            Term::Record { fields, .. } => stack.extend(fields.iter().map(|(_, v)| v)),
            Term::Local { .. } | Term::Column { .. } | Term::Lit { .. } => {}
        }
    }
    Ok(())
}

/// Lower a window specification term into the checked layer.
///
/// The **field rules** are not restated here: which fields a spec may have is
/// `rules::winspec`, and the structural validity of the result is
/// `checked::window_spec`. What this function does is the part the term layer
/// owns — reading an already-resolved *term* rather than re-parsing source.
///
/// Order keys are the reason this cannot simply hand the whole record back as
/// syntax. `asc e` and `desc e` are prelude **primitives** (`__asc`, `__desc`),
/// so by the time a term exists the direction is a resolved primitive wrapping
/// an expression — and recovering it from the name is precisely the
/// syntax-shape dispatch this migration removes. Reading it from the term is
/// the same fact, obtained once.
pub fn lower_winspec(t: &Term) -> Result<crate::checked::WinSpecChecked, Gap> {
    let Term::Record { fields, origin } = t else {
        return Err(Gap::new(
            other_origin(t),
            "a window specification must be a record `{ partition = .., order = .., frame = .. }`",
        ));
    };
    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    for (key, value) in fields {
        match key.as_str() {
            crate::rules::winspec::PARTITION => {
                let Term::List { items, .. } = value else {
                    return Err(Gap::new(
                        other_origin(value),
                        "`partition` must be a list of expressions",
                    ));
                };
                partition = lower_exprs(items)?;
            }
            crate::rules::winspec::ORDER => {
                let Term::List { items, .. } = value else {
                    return Err(Gap::new(
                        other_origin(value),
                        "`order` must be a list of sort keys",
                    ));
                };
                for item in items {
                    order.push(lower_sortkey(item)?);
                }
            }
            crate::rules::winspec::FRAME => {
                frame = Some(lower_frame(value)?);
            }
            other => {
                return Err(Gap::new(
                    *origin,
                    format!(
                        "unknown window spec field `{other}`; expected {}",
                        crate::rules::winspec::names()
                    ),
                ))
            }
        }
    }
    crate::checked::window_spec(partition, order, frame).map_err(|e| Gap::new(*origin, e.message))
}

/// Lower one sort key: `asc e`, `desc e`, or a bare expression (ascending).
fn lower_sortkey(t: &Term) -> Result<(crate::checked::CheckedExpr, bool), Gap> {
    // The direction is a resolved primitive wrapping exactly one argument.
    if let Term::Primitive { name, args, .. } = t {
        if args.len() == 1 {
            let ascending = match &**name {
                "__asc" => Some(true),
                "__desc" => Some(false),
                _ => None,
            };
            if let Some(ascending) = ascending {
                return Ok((lower_expr(&args[0])?, ascending));
            }
        }
    }
    Ok((lower_expr(t)?, true))
}

/// Lower a frame: `rows a b`, or one of the prelude's named frames.
///
/// Resolved through the term, so a name bound to a frame is already its body —
/// the case that needed a scope lookup here before.
fn lower_frame(t: &Term) -> Result<crate::ir::Frame, Gap> {
    let origin = other_origin(t);
    let (name, args) = match t {
        Term::Primitive { name, args, .. } => (&**name, args.as_slice()),
        _ => {
            return Err(Gap::new(
                origin,
                "a frame must be `rows a b`, `range a b`, or `groups a b`",
            ))
        }
    };
    // The IR carries only the two bounds — the `ROWS`/`RANGE`/`GROUPS` keyword
    // is not represented, matching `ir::Frame` and the source path.
    match name {
        "__rows" => {}
        other => {
            return Err(Gap::new(
                origin,
                format!("`{other}` is not a frame kind this layer can read"),
            ))
        }
    }
    let [start, end] = args else {
        return Err(Gap::new(origin, "a frame needs two bounds"));
    };
    // Built through the checked constructor rather than as a bare struct: it is
    // the one place that knows which frames are impossible, and assembling the
    // IR directly here skipped that rule — `rows (following 1) (preceding 1)`
    // reached SQL as `ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING`, which the
    // engines reject. A rejection is the program's, not this phase's, so it is
    // reported as one.
    crate::checked::frame(lower_bound(start)?, lower_bound(end)?)
        .map_err(|e| Gap::program(origin, e.message))
}

/// Lower one frame bound: `unboundedPreceding`, `currentRow`,
/// `unboundedFollowing`, or `preceding n` / `following n`.
fn lower_bound(t: &Term) -> Result<crate::ir::Bound, Gap> {
    let origin = other_origin(t);
    let (name, args) = match t {
        Term::Primitive { name, args, .. } => (&**name, args.as_slice()),
        _ => return Err(Gap::new(origin, "a frame bound must be a bound")),
    };
    Ok(match (name, args) {
        ("__unboundedPreceding", []) => crate::ir::Bound::UnboundedPreceding,
        ("__unboundedFollowing", []) => crate::ir::Bound::UnboundedFollowing,
        ("__currentRow", []) => crate::ir::Bound::CurrentRow,
        ("__preceding", [n]) => crate::ir::Bound::Preceding(frame_offset(n)?),
        ("__following", [n]) => crate::ir::Bound::Following(frame_offset(n)?),
        _ => {
            return Err(Gap::new(
                origin,
                format!("`{name}` is not a frame bound this layer can read"),
            ))
        }
    })
}

/// The integer a frame offset term denotes.
fn frame_offset(t: &Term) -> Result<i64, Gap> {
    match t {
        Term::Lit {
            lit: crate::ir::Lit::Int(i),
            ..
        } => Ok(*i),
        // A frame offset is a *literal* by the language's rules, so a computed
        // one is refused rather than evaluated — matching the source path, which
        // reads the literal too.
        other => Err(Gap::new(
            other_origin(other),
            "a frame offset must be a whole-number literal",
        )),
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
    lower_expr_with(t, &|_| {
        Err(Gap::new(
            Origin::new(0, cagara_syntax::ast::Span::default()),
            "a window specification has no lowering here",
        ))
    })
}

/// Lower a term, completing window applications with `spec`.
///
/// The hook exists because a window's spec is a **query-level** record with its
/// own rules — which fields exist, that an order key must name a column, what a
/// frame bound may be — and those rules are a separate lowering
/// ([`lower_winspec`]) rather than part of this walk. Handing the spec's term
/// back is what keeps them in one place: the term layer carries the spec's
/// *structure*, and this walk never reads it a second way.
///
/// A caller with no window support passes a hook that refuses, which is what
/// [`lower_expr`] does. That is right for its callers: they lower a window's
/// **specification** fields — partition expressions, order keys, frame bounds —
/// where a nested window is not a thing the language allows, so a refusal is the
/// correct answer rather than a gap to be filled in.
pub fn lower_expr_with(
    t: &Term,
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<crate::checked::CheckedExpr, Gap> {
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
            let lowered = lower_exprs_with(args, spec)?;
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
        Term::Window {
            sql,
            spec: spec_term,
            args,
            ty,
            origin,
        } => {
            let lowered_spec = spec(spec_term)?;
            let values = lower_exprs_with(args, spec)?;
            C::win_template(sql.to_string(), values, lowered_spec, ty.clone(), *origin)
                .map_err(|e| Gap::new(*origin, e.message))
        }
        Term::Primitive {
            name,
            args,
            ty: _,
            row: _,
            origin,
        } => lower_primitive(name, args, *origin, spec),
        Term::Case {
            arms,
            default,
            ty,
            origin,
        } => {
            // The searched `CASE` the prelude's `ifThenElse` emits. Built here
            // rather than reached through a name because the arity depends on
            // how many arms were written, which no fixed-arity definition can
            // express.
            // A `case` whose only arm is the default **is** that default.
            // Emitting it as a `CASE` would need a `WHEN`, and there is no
            // condition to test — `CASE ELSE $1 END` is not SQL. The source path
            // had this rule; dropping it in the migration turned `case { true =>
            // x }` into a parse error.
            if arms.is_empty() {
                return lower_expr_with(default, spec);
            }
            let mut sql = String::from("CASE");
            let mut lowered: Vec<crate::checked::CheckedExpr> = Vec::new();
            for (cond, value) in arms {
                sql.push_str(" WHEN $");
                sql.push_str(&(lowered.len() + 1).to_string());
                lowered.push(lower_expr_with(cond, spec)?);
                sql.push_str(" THEN $");
                sql.push_str(&(lowered.len() + 1).to_string());
                lowered.push(lower_expr_with(value, spec)?);
            }
            sql.push_str(" ELSE $");
            sql.push_str(&(lowered.len() + 1).to_string());
            lowered.push(lower_expr_with(default, spec)?);
            sql.push_str(" END");
            C::template(sql, lowered, ty.clone(), *origin).map_err(|e| Gap::new(*origin, e.message))
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

/// Lower a sequence of terms, forwarding a window-spec hook.
fn lower_exprs_with(
    ts: &[Term],
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<Vec<crate::checked::CheckedExpr>, Gap> {
    ts.iter().map(|t| lower_expr_with(t, spec)).collect()
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
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<crate::checked::CheckedExpr, Gap> {
    use crate::checked::CheckedExpr as C;
    match (name, args) {
        // `group key`: an aggregate-phase node whose type is the key's.
        ("__group", [key]) => {
            C::group(lower_expr_with(key, spec)?, origin).map_err(|e| Gap::new(origin, e.message))
        }
        // `inList [..] .x`: the list comes first, then the value, matching the
        // prelude's declaration. Swapping them can still produce a well-typed
        // tree, so the order is stated explicitly.
        ("__in", [list, value]) => {
            let items = match list {
                Term::List { items, .. } => lower_exprs_with(items, spec)?,
                other => {
                    return Err(Gap::new(
                        other_origin(other),
                        "the first argument of `in` must be a list",
                    ))
                }
            };
            C::in_(lower_expr_with(value, spec)?, items, false, origin)
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

/// The gap a cyclic definition produces.
///
/// One constructor for both inlining sites so the two cannot drift: a cycle must
/// read the same way whichever route reached it, and the wording matches the
/// elaborator's `enter`, so both layers agree on what recursion is.
fn recursion_gap(origin: Origin, name: &str) -> Gap {
    Gap::new(
        origin,
        format!("`{name}` refers to itself; recursion is not supported"),
    )
}

/// The gap for an inline that did not happen: a cycle or the depth limit.
///
/// `inlining` returns `None` for both, so the two are told apart by the flag the
/// depth check sets. Reporting a chain as "refers to itself" would be a lie the
/// user cannot act on — a chain is not a cycle, and the fix is to shorten it
/// rather than to remove a self-reference. The depth limit is a **program**
/// fault for the same reason: the program's shape is what is too deep, so it is
/// reported as the user's to fix rather than as an internal error.
fn inline_limit_gap(cx: &Normalizer<'_>, origin: Origin, name: &str) -> Gap {
    if cx.hit_depth_limit() {
        Gap::program(
            origin,
            format!(
                "`{name}` is more than {MAX_INLINE_DEPTH} definitions deep; \
                 split the pipeline into shorter stages"
            ),
        )
    } else {
        recursion_gap(origin, name)
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
/// The single reader of this rule. Elaboration used to carry the same three-case
/// match over the same three strings, with its own copy of the enum, justified
/// by an analogy to `lit_of` — but the two are not alike. `lit_of` translates
/// between two `Lit` enums that *should* stay independent, so writing it out
/// makes a new AST variant a compile error there. This reads three `head`
/// strings out of a type expression and answers a question both layers ask; a
/// fourth kind added to one side and not the other would have been two silently
/// different answers, not a compile error.
///
/// Read from the type expression rather than the resolved scheme because only
/// the declared form distinguishes `expr r a` from `agg (expr r a)`; after
/// resolution both are just the result type.
pub(crate) fn declared_template_kind(d: &cagara_syntax::ast::Def) -> TemplateKind {
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
/// The two `Lit` enums have identical shape, so this is a pure translation; it
/// is written out rather than derived so that a variant added to the AST is a
/// compile error here instead of a silently dropped literal. (The
/// `elaborate::lit_of` this once mirrored went with the source path; every
/// literal now reaches the checked layer through this one.)
fn lit_of(l: &cagara_syntax::ast::Lit) -> crate::ir::Lit {
    use cagara_syntax::ast::Lit as L;
    match l {
        L::Int(i) => crate::ir::Lit::Int(*i),
        L::Float(f) => crate::ir::Lit::Float(f.clone()),
        L::Str(s) => crate::ir::Lit::Str(s.clone()),
        L::Bool(b) => crate::ir::Lit::Bool(*b),
    }
}

/// [`Facts`] backed by a real workspace, for tests.
///
/// Lives in the library rather than in `#[cfg(test)]` because integration tests
/// under `tests/` are a separate crate: they need a [`Facts`] they can build from
/// a [`Workspace`](crate::workspace::Workspace) and a
/// [`TypeCheck`](crate::check::TypeCheck), and a `#[cfg(test)]` item is not
/// visible to them. `lower_query_agreement` is the main user.
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
    /// The assignment resolved for the callee at this use site.
    ///
    /// Overridden rather than left to the trait's default, for the reason
    /// [`resolved_assignment`] gives: an adapter with a checker in hand that
    /// answers "nothing recorded" makes every hole open and tests a route
    /// production never takes.
    fn holes_at(&self, (module, owner): (usize, usize), site: u32) -> Vec<(usize, usize)> {
        resolved_assignment(self.input.type_check(), module, owner, site)
    }

    fn resolution(&self, module: usize, id: u32) -> Option<Resolved> {
        self.input.type_check().resolution(module, id).cloned()
    }

    fn def_body(&self, module: usize, def: usize) -> Option<Body<'_>> {
        let d = self.input.module(module).source().defs.get(def)?;
        Some(Body {
            module,
            def,
            expr: &d.body,
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
        choice_at(self.input.type_check(), module, owner, site)
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

    fn use_row(&self, module: usize, id: u32) -> Option<Vec<(String, crate::core::ScalarType)>> {
        self.input
            .type_check()
            .typed_node(module, id)
            .and_then(|node| node.row())
    }
}

impl Facts for WorkspaceFacts<'_> {
    fn resolution(&self, module: usize, id: u32) -> Option<Resolved> {
        self.tc.resolution(module, id).cloned()
    }

    /// The same answer the compiler's adapter gives, from the same function.
    ///
    /// This used to fall back to the trait's default — no assignment, every hole
    /// open — which made the test adapter and the production adapter disagree
    /// about a question the walk depends on.
    fn holes_at(&self, (module, owner): (usize, usize), site: u32) -> Vec<(usize, usize)> {
        resolved_assignment(self.tc, module, owner, site)
    }

    fn def_body(&self, module: usize, def: usize) -> Option<Body<'_>> {
        let d = self.ws.modules.get(module)?.module.defs.get(def)?;
        Some(Body {
            module,
            def,
            expr: &d.body,
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
        choice_at(self.tc, module, owner, site)
    }

    fn def_result_ty(&self, module: usize, def: usize) -> Option<crate::core::ScalarType> {
        self.tc.result_scalar(module, def)
    }

    fn use_ty(&self, module: usize, id: u32) -> crate::core::ScalarType {
        self.tc
            .use_ty(module, id)
            .unwrap_or(crate::core::ScalarType::Unknown)
    }

    fn use_row(&self, module: usize, id: u32) -> Option<Vec<(String, crate::core::ScalarType)>> {
        self.tc.typed_node(module, id).and_then(|node| node.row())
    }
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

    /// A binder that the argument mentions free is renamed, so the argument is
    /// not captured by it.
    ///
    /// `substitute` matches binders by name, which is only sound while no two
    /// binders in one term share a name. Inlining breaks that: two definitions
    /// inlined into one term can each bind `q`, and the use site of one can be a
    /// binder of the other. Without the rename, `wrap 1` under `h`'s own `q`
    /// captured the argument and `h 1 2` reduced to `2` instead of `1`.
    #[test]
    fn an_argument_is_not_captured_by_a_binder_of_the_same_name() {
        // `wrap = p => (q => p)`, inlined at a site whose own binder is `q`.
        let wrap = lam(&["p"], lam(&["q"], local("p")));
        let applied = wrap.apply(local("q")).unwrap();
        // `applied` is `q => q`, but its `q` is `wrap`'s — renaming keeps it
        // from being confused with the caller's `q`.
        let caller = lam(&["q"], applied);
        let step = caller.apply(local("first")).unwrap();
        // The caller's `q` was captured by nothing: the inner lambda still
        // stands for `wrap`'s parameter, so applying it yields `first`.
        let done = step.apply(local("second")).unwrap();
        assert_eq!(done, local("first"));
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
            expr: &ws.modules[ws.root].module.defs[def].body,
        };
        let resolved = |module: usize, id: u32| facts.resolution(module, id);
        let mut cx = Normalizer::new(Box::new(resolved));
        cx.in_module(ws.root, |cx| expression(&facts, cx, &body))
    }

    /// A top-level definition inlined inside a lambda is not read as that
    /// lambda's parameter, even when the names match.
    ///
    /// A definition's body is closed, so the caller's binders are not in scope
    /// in it. Leaving them on the binder stack made `n = 5`, `g = n + 1`,
    /// `f = n => g`, `f 1` normalize `n` to the argument: the emitted SQL was
    /// `1 + 1` where the answer is `6`.
    #[test]
    fn an_inlined_body_does_not_see_the_callers_binders() {
        let t = term_of(
            "n = 5\n\
             g = n\n\
             f = n => g\n\
             main = users & select { y = f 1 }\n",
            "f",
        )
        .unwrap();
        // `f` is a function whose body is the inlined `g`, i.e. the top-level
        // `n`. Reading that `n` as `f`'s own parameter is the capture this test
        // pins; the body must hold a literal instead.
        let Term::Lambda { body, .. } = t else {
            panic!("`f` is a function");
        };
        assert!(
            !mentions_local(&body),
            "the parameter `n` was captured by the inlined body: {body:?}"
        );
    }

    /// Whether a term mentions any [`Term::Local`].
    fn mentions_local(t: &Term) -> bool {
        match t {
            Term::Local { .. } => true,
            Term::Lambda { body, .. } => mentions_local(body),
            Term::Apply { head, args, .. } => {
                mentions_local(head) || args.iter().any(mentions_local)
            }
            Term::Template { args, .. }
            | Term::Primitive { args, .. }
            | Term::List { items: args, .. } => args.iter().any(mentions_local),
            Term::Window { spec, args, .. } => {
                mentions_local(spec) || args.iter().any(mentions_local)
            }
            Term::Case { arms, default, .. } => {
                arms.iter()
                    .any(|(c, v)| mentions_local(c) || mentions_local(v))
                    || mentions_local(default)
            }
            Term::Record { fields, .. } => fields.iter().any(|(_, v)| mentions_local(v)),
            Term::Column { .. } | Term::Lit { .. } => false,
        }
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
            Term::Primitive {
                name,
                args,
                ty,
                row,
                ..
            } => Term::Primitive {
                name,
                args: args.iter().map(strip_origins).collect(),
                ty,
                row,
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
            Term::Case {
                arms, default, ty, ..
            } => Term::Case {
                arms: arms
                    .iter()
                    .map(|(c, v)| (strip_origins(c), strip_origins(v)))
                    .collect(),
                default: Box::new(strip_origins(&default)),
                ty,
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

    /// A self-naming definition reports recursion rather than inlining forever.
    ///
    /// The term layer **inlines** a definition instead of calling it, so
    /// `f = f` would expand without end. The elaborator guards this in `enter`
    /// with `Ctx::active`; a walk that builds terms needs the same rule, and
    /// when it was missing the failure was a stack **overflow** — an abort, not
    /// a diagnostic, which is why it went unnoticed until a SQL test happened to
    /// exercise it.
    #[test]
    fn a_self_reference_is_reported_not_inlined_forever() {
        // `f = f` is a value-level alias cycle; `g`/`h` is a two-step one.
        for (src, name) in [
            ("c = .name\nf = f\nq = users & select { x = f }\n", "f"),
            (
                "c = .name\nf = g\ng = f\nq = users & select { x = f }\n",
                "f",
            ),
        ] {
            let ws = Workspace::from_source(&format!("{TABLES}{src}"));
            let tc = check(&ws);
            let facts = WorkspaceFacts::new(&ws, &tc);
            let defs = &ws.modules[ws.root].module.defs;
            let index = defs.iter().position(|d| d.name == name).expect("the alias");
            let body = Body {
                module: ws.root,
                def: index,
                expr: &defs[index].body,
            };
            let resolved = |module: usize, id: u32| facts.resolution(module, id);
            let mut cx = Normalizer::new(Box::new(resolved));
            let out = cx.in_module(ws.root, |cx| expression(&facts, cx, &body));
            match out {
                Err(gap) => assert!(
                    gap.message.contains("refers to itself"),
                    "a cycle should be named as recursion, got: {}",
                    gap.message
                ),
                Ok(t) => panic!("a cyclic definition should not normalize, got {t:?}"),
            }
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
                    expr: &d.body,
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
                    expr: &d.body,
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

// ── lowering a term that denotes a *query* ──────────────────────────────────

/// Lower a term that denotes a **query** into the checked relational layer.
///
/// The relational half of [`lower_expr_with`], and it exists for the same
/// reason: by the time a term exists, every name is resolved, every application
/// is saturated, and the argument order is the one the prelude's declaration
/// gives — the pipe, the operator sugar, aliases and composition are all gone.
/// So this matches on the *primitive* each stage was reached through, exactly as
/// [`lower_primitive`] matches on the expression ones, and hands the pieces to a
/// `CheckedQuery` constructor that already states the rule for that stage.
///
/// Every stage's arguments are in **declaration order with the piped input
/// last** — `__where [pred, input]`, `__select [fields, input]`,
/// `__innerJoin [right, on, input]` — which is what the normalizer produces and
/// what the prelude says. Nothing here re-derives that order from syntax, and
/// nothing here re-reads a name.
///
/// `spec` completes window applications, as in the expression lowering.
pub fn lower_query(t: &Term) -> Result<crate::checked::CheckedQuery, Gap> {
    // A query lowering **must** complete window specifications: a window is
    // written in a `select`'s field, so it is reached through this walk rather
    // than beside it. `lower_winspec` is the same hook the production expression
    // path passes, so the spec's rules — which fields exist, that an order key
    // must name a column, what a frame bound may be — keep one implementation
    // instead of growing a second reading here.
    lower_query_with(t, &lower_winspec)
}

/// Lower a query term, completing window specs with `spec`.
pub fn lower_query_with(
    t: &Term,
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<crate::checked::CheckedQuery, Gap> {
    let Term::Primitive {
        name,
        args,
        row,
        origin,
        ..
    } = t
    else {
        return Err(Gap::new(
            other_origin(t),
            "a query term must be a stage application or a table",
        ));
    };
    let built = lower_query_primitive(
        name,
        args,
        row.as_ref().map(|r| r.as_slice()),
        *origin,
        spec,
    )?;
    Ok(built)
}

fn lower_query_primitive(
    name: &str,
    args: &[Term],
    row: Option<&[(String, crate::core::ScalarType)]>,
    origin: Origin,
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<crate::checked::CheckedQuery, Gap> {
    use crate::checked::CheckedQuery as Q;
    // A constructor's rejection is carried through **with its fault**, not
    // flattened to a capability gap. Every rule the checked layer states —
    // a column that is not there, a predicate that is not bool, inputs with
    // different columns — is the *program's* to fix, and reporting it as "this
    // phase cannot build that" would send the caller down its fallback route
    // instead of telling the user. The wording is unchanged either way; the
    // classification is what decides who hears it.
    let at = |e: crate::core::Error| {
        let mut gap = Gap::new(origin, e.message);
        gap.fault = e.fault;
        gap
    };
    // The piped input is the **last** argument of every stage, and a stage's own
    // arguments are in front of it, in declaration order.
    let split = |n: usize| -> Result<(&[Term], &Term), Gap> {
        if args.len() != n + 1 {
            return Err(Gap::new(
                origin,
                format!("`{name}` takes {n} argument(s) and a piped input"),
            ));
        }
        Ok((&args[..n], &args[n]))
    };
    let lower_input = |t: &Term| lower_query_with(t, spec);
    let expr = |t: &Term| lower_expr_with(t, spec);

    match name {
        // `table "s" "t"`: the only leaf, and the only place a row is needed
        // that the arguments do not carry.
        "__table" => {
            let (s, n) = match args {
                [s, n] => (string_of(s), string_of(n)),
                _ => (None, None),
            };
            let (Some(schema), Some(table)) = (s, n) else {
                return Err(Gap::new(origin, "`table` takes a schema and a name"));
            };
            let columns = row.map(|r| crate::core::RowType::new(r.to_vec()));
            Q::table(schema, table, columns, origin).map_err(at)
        }
        "__where" => {
            let ([pred], input) = split(1)? else {
                unreachable!()
            };
            Q::where_(lower_input(input)?, expr(pred)?, origin).map_err(at)
        }
        "__select" => {
            let ([fields], input) = split(1)? else {
                unreachable!()
            };
            Q::select(lower_input(input)?, fields_of(fields, spec)?, origin).map_err(at)
        }
        "__update" => {
            let ([fields], input) = split(1)? else {
                unreachable!()
            };
            Q::update(lower_input(input)?, fields_of(fields, spec)?, origin).map_err(at)
        }
        "__agg" => {
            let ([fields], input) = split(1)? else {
                unreachable!()
            };
            Q::agg(lower_input(input)?, fields_of(fields, spec)?, origin).map_err(at)
        }
        "__order" => {
            let ([keys], input) = split(1)? else {
                unreachable!()
            };
            Q::order(lower_input(input)?, sort_keys(keys, spec)?, origin).map_err(at)
        }
        "__limit" => {
            let ([n], input) = split(1)? else {
                unreachable!()
            };
            Q::limit(lower_input(input)?, int_of(n, "limit", origin)?, origin).map_err(at)
        }
        "__offset" => {
            let ([n], input) = split(1)? else {
                unreachable!()
            };
            Q::offset(lower_input(input)?, int_of(n, "offset", origin)?, origin).map_err(at)
        }
        "__distinct" => {
            let ([], input) = split(0)? else {
                unreachable!()
            };
            Q::distinct(lower_input(input)?, origin).map_err(at)
        }
        "__omit" => {
            let ([k], input) = split(1)? else {
                unreachable!()
            };
            let Some(key) = string_of(k) else {
                return Err(Gap::new(origin, "`omit` takes a column name"));
            };
            Q::omit(lower_input(input)?, key, origin).map_err(at)
        }
        "__mapValue" => {
            let ([wrapper], input) = split(1)? else {
                unreachable!()
            };
            let Some(wrapper) = string_of(wrapper) else {
                return Err(Gap::new(origin, "`mapValue` takes a wrapper name"));
            };
            Q::map_value(lower_input(input)?, wrapper, origin).map_err(at)
        }
        "__prefix" | "__suffix" => {
            let ([a], input) = split(1)? else {
                unreachable!()
            };
            let Some(affix) = string_of(a) else {
                return Err(Gap::new(origin, format!("`{name}` takes a string")));
            };
            let q = lower_input(input)?;
            if name == "__prefix" {
                Q::prefix(q, affix, origin).map_err(at)
            } else {
                Q::suffix(q, affix, origin).map_err(at)
            }
        }
        // A join: `__innerJoin [right, on, input]`. The **piped input is last**
        // and the joined input is first, which is the prelude's declaration
        // order (`innerJoin : query s -> expr (join r s) bool -> query r -> …`)
        // with the trailing argument supplied by the pipe. Nothing here decides
        // which side is which: the term says, and `users & orders ? on`
        // normalizes to the same node, so the operator and named spellings
        // cannot diverge.
        "__innerJoin" | "__leftJoin" | "__rightJoin" | "__fullJoin" | "__semiJoin"
        | "__antiJoin" => {
            let [right, on, input] = args else {
                return Err(Gap::new(
                    origin,
                    format!("`{name}` takes a right input, a predicate and a piped input"),
                ));
            };
            let kind = match name {
                "__innerJoin" => crate::ir::JoinKind::Inner,
                "__leftJoin" => crate::ir::JoinKind::Left,
                "__rightJoin" => crate::ir::JoinKind::Right,
                "__fullJoin" => crate::ir::JoinKind::Full,
                "__semiJoin" => crate::ir::JoinKind::Semi,
                _ => crate::ir::JoinKind::Anti,
            };
            Q::join(
                kind,
                lower_input(input)?,
                lower_input(right)?,
                expr(on)?,
                origin,
            )
            .map_err(at)
        }
        // A set operation: `__union [left, right]`, in **argument order**, which
        // is the primitive's own declaration order. The two spellings really do
        // differ and the term records the difference rather than smoothing it:
        //
        //     users & union vips   ->  __union [vips, users]
        //     users &| vips        ->  __union [users, vips]
        //
        // because `union` is `query r -> query r -> query r` at the pipe's own
        // level, so the piped query arrives last, while `&|` is
        // `q => r => union q r` (`prelude.cagara:153`), where the body re-binds
        // and the piped query ends up first. Reading `args[0]` as the left
        // operand is therefore not a convention chosen here — it is what the
        // term says. Inferring the side from which spelling was written is what
        // reversed `except` once already.
        "__union" | "__unionAll" | "__intersect" | "__except" => {
            let [left, right] = args else {
                return Err(Gap::new(origin, format!("`{name}` takes two queries")));
            };
            let kind = match name {
                "__union" => crate::ir::SetKind::Union,
                "__unionAll" => crate::ir::SetKind::UnionAll,
                "__intersect" => crate::ir::SetKind::Intersect,
                _ => crate::ir::SetKind::Except,
            };
            Q::set(kind, lower_input(left)?, lower_input(right)?, origin).map_err(at)
        }
        _ => Err(Gap::new(
            origin,
            format!("the relational primitive `{name}` is not lowered yet"),
        )),
    }
}

/// The fields of a `select`/`update`/`agg` record term, in source order.
fn fields_of(
    t: &Term,
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<Vec<(String, crate::checked::CheckedExpr)>, Gap> {
    let Term::Record { fields, .. } = t else {
        return Err(Gap::new(
            other_origin(t),
            "a projection's fields must be a record",
        ));
    };
    fields
        .iter()
        .map(|(n, v)| Ok((n.clone(), lower_expr_with(v, spec)?)))
        .collect()
}

/// The keys of an `order` list term, with each key's direction.
///
/// The direction is carried by the resolved `__asc`/`__desc` primitive rather
/// than by re-reading the name out of source, which is what makes the two
/// spellings of a sort agree.
fn sort_keys(
    t: &Term,
    spec: &dyn Fn(&Term) -> Result<crate::checked::WinSpecChecked, Gap>,
) -> Result<Vec<(crate::checked::CheckedExpr, bool)>, Gap> {
    let Term::List { items, .. } = t else {
        return Err(Gap::new(other_origin(t), "`order` takes a list of keys"));
    };
    items
        .iter()
        .map(|item| match item {
            Term::Primitive { name, args, .. } if &**name == "__asc" || &**name == "__desc" => {
                let [key] = &args[..] else {
                    return Err(Gap::new(
                        other_origin(item),
                        "a sort key takes one expression",
                    ));
                };
                Ok((lower_expr_with(key, spec)?, &**name == "__asc"))
            }
            other => Ok((lower_expr_with(other, spec)?, true)),
        })
        .collect()
}

/// The text of a string literal term.
fn string_of(t: &Term) -> Option<String> {
    match t {
        Term::Lit {
            lit: crate::ir::Lit::Str(s),
            ..
        } => Some(s.clone()),
        _ => None,
    }
}

/// The value of a whole-number literal term.
/// The count a `limit`/`offset` stage was given, as a literal.
///
/// A **program** rule rather than a gap, and the distinction is load-bearing: a
/// gap tells the caller "this phase cannot build that", which sends the
/// definition down the fallback route and lets the source path report the
/// mistake. The mistake is the program's, and it reads the same either way, so
/// it is reported here.
///
/// # Why this does not name the form it found
///
/// The source path said `` `limit` expects an int, found an application ``,
/// because it held the syntax tree and could name the node. This route cannot,
/// and that is not an oversight to repair: `1 + 1` normalizes to a `Template`
/// (`sql "$1 + $2"` applied), so by the time the lowering sees it, "it was
/// written as an application" is precisely the information normalization exists
/// to discard. A term-kind match here would answer "a sql template" — true of
/// the term and useless to a reader, while leaking internal vocabulary at someone
/// who wrote `1 + 1`.
///
/// So the message states the requirement rather than describing the input. That
/// is actionable whatever was found, and it is honest about what this layer can
/// see. The **span** is carried from the argument's own origin, so the caret
/// lands on the expression the user wrote rather than on `limit`; that is the
/// part that guides a reader, and it matches the source path exactly — verified
/// by a differential run against the pre-migration compiler over twenty error
/// programs, which is also what caught the original wording being too thin.
///
/// A negative count is caught by the checked constructor instead, so it is not
/// restated here.
fn int_of(t: &Term, what: &str, origin: Origin) -> Result<i64, Gap> {
    match t {
        Term::Lit {
            lit: crate::ir::Lit::Int(n),
            ..
        } => Ok(*n),
        other => {
            // The argument's own origin, so the caret is on the expression the
            // user wrote rather than on the stage that wanted an int. Falls back
            // to the stage's span when the term carries none.
            let at = other_origin(other);
            let at = if at.span == cagara_syntax::ast::Span::default() {
                origin
            } else {
                at
            };
            // "expects an int" states the type the checker would also want;
            // "literal" states the rule only this layer knows — a column or an
            // arithmetic expression is an int and still not a count. Both are
            // needed, so both are said.
            Err(Gap::program(at, format!("`{what}` expects an int literal")))
        }
    }
}
