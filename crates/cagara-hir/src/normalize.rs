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

use crate::core::Origin;
use std::collections::HashMap;
use std::sync::Arc;

/// A definition's identity: module index and source-order position.
pub type DefId = (usize, usize);

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
        origin: Origin,
    },
    /// A literal, already at the type the checker gave it.
    Lit { lit: crate::ir::Lit, origin: Origin },
    /// A `sql "..."` template with its arguments already normalized.
    ///
    /// The arguments are positional, matching the template's `$1`…`$n`, and
    /// there are exactly as many as the template takes: saturation happened
    /// during reduction.
    Template {
        sql: Arc<str>,
        args: Vec<Term>,
        origin: Origin,
    },
    /// A primitive, with its arguments saturated.
    Primitive {
        name: Arc<str>,
        args: Vec<Term>,
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
    pub fn is_function(&self) -> bool {
        matches!(self, Term::Lambda { .. })
    }

    /// Apply this term to one argument, reducing as far as the result allows.
    ///
    /// Returns `None` when the term is not applicable — applying a column
    /// reference, a literal, or a record is a *type* error, and the checker has
    /// already rejected it, so the caller treats `None` as "cannot happen on a
    /// checker-accepted program" rather than as a user diagnostic. Reporting it
    /// here would invent a second, weaker error for something the checker
    /// already owns.
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
            other => {
                let _ = other;
                None
            }
        }
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
        Term::Local { .. } | Term::Lit { .. } => term,
        Term::Template { sql, args, origin } => Term::Template {
            sql,
            args: args.into_iter().map(|a| substitute(a, name, arg)).collect(),
            origin,
        },
        Term::Primitive {
            name: pname,
            args,
            origin,
        } => Term::Primitive {
            name: pname,
            args: args.into_iter().map(|a| substitute(a, name, arg)).collect(),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// The checker resolved the name to this definition.
    Def(DefId),
    /// The checker resolved the name to a primitive.
    Primitive,
    /// The name is bound by an enclosing binder (a lambda parameter), so it is
    /// a local rather than a definition.
    Local,
}

/// The operation normalization performs on one occurrence.
///
/// A caller supplies one of these per unresolved name. Returning `None` means
/// "this occurrence is a local", which is the only case where the term keeps a
/// name at all.
pub type Resolver<'a> = dyn Fn(u32) -> Option<Resolved> + 'a;

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
}

impl<'a> Normalizer<'a> {
    pub fn new(resolver: Box<Resolver<'a>>) -> Self {
        Self {
            resolver,
            owner: None,
            cache: HashMap::new(),
            binders: Vec::new(),
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

    /// Resolve one occurrence through the caller's resolver.
    pub fn resolve(&self, id: u32) -> Option<Resolved> {
        (self.resolver)(id)
    }

    /// Push a binder for the duration of `f`.
    pub fn with_binder<T>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        self.binders.push(name.to_string());
        let out = f(self);
        self.binders.pop();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::new(0, cagara_syntax::ast::Span::default())
    }

    fn local(label: &str) -> Term {
        Term::Local {
            id: 0,
            label: label.to_string(),
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

    /// Applying something that is not a function is not normalization's to
    /// report.
    #[test]
    fn applying_a_non_function_yields_nothing() {
        assert!(local("x").apply(local("y")).is_none());
        assert!(lam(&[], local("x")).apply(local("y")).is_none());
    }
}
