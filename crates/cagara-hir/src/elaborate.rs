//! Source elaboration: build the checked tree directly from source.
//!
//! Type checking supplies the facts used to construct `CheckedQuery` directly
//! from source. The result is erased only at the SQL boundary.
//!
//! # What it reads, and what it does not compute
//!
//! It does **not** re-derive types. Everything it needs is recorded by the
//! checker and read through `TypeCheck`:
//!
//! * `use_ty(module, ExprId)` — the scalar type of one expression;
//! * `scheme_view` / `scheme_fields` — the row shape of a definition;
//! * `choices_of` — which overload candidate a use resolved to;
//! * `rules::is_pipe_name` and the prelude's stage declarations — which
//!   operator is a pipeline stage.
//!
//! Re-deriving any of those would create a second source of truth for types.
//!
//! # Scope
//!
//! Expressions are elaborated for the *scalar* cases a `CheckedExpr` can hold:
//! columns, literals, SQL templates, `in`, and `group`. An expression whose
//! type the checker did not record as a scalar — a query, a function, a nested
//! stage — is reported as an error rather than given an invented type, because
//! a `CheckedExpr` carrying a made-up `ScalarType` is exactly the kind of
//! plausible-but-wrong value this layer must not produce.

use crate::check::{Choice, TypeCheck};
use crate::checked::{CheckedExpr, CheckedQuery};
use crate::compile::CompilerInput;
use crate::core::{Error, Origin, ScalarType};
use crate::ir::{Lit, Phase};
use crate::rules;
use crate::workspace::Binding;
use cagara_syntax::ast::{self, ExprKind, Span};

#[cfg(test)]
thread_local! {
    static ELABORATE_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn elaborate_runs() -> usize {
    ELABORATE_RUNS.with(|runs| runs.get())
}

/// What every elaboration step needs: immutable compiler input, the module and
/// definition being elaborated, the scope names resolve in, and local bindings.
/// Keeping this context as one value prevents source access from bypassing the
/// compiler boundary as the elaborator descends into imported definitions.
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub input: CompilerInput<'a>,
    /// The module the expression being elaborated lives in.
    pub module: usize,
    /// The definition whose body it belongs to. Overload choices are recorded
    /// per definition, keyed by the use's `ExprId`, so resolving one needs the
    /// owner as well as the site.
    pub owner: usize,
    pub scope: &'a std::collections::HashMap<String, Binding>,
    /// Parameters bound by an enclosing application, innermost last.
    ///
    /// Lambda parameters are bound to elaborated arguments here, shadowing the
    /// module scope. Values may be queries, expressions, callables, or stages.
    ///
    /// A `Vec` of pairs rather than a map, because a name may be shadowed by a
    /// nearer binding and the innermost must win, which a plain map would lose.
    pub env: &'a [(String, CheckedValue)],
    /// The overload candidates chosen for the definition currently being
    /// elaborated, indexed by hole.
    ///
    /// A polymorphic definition's overloaded sites are resolved at each use,
    /// not where they are written.
    ///
    /// `h = e => users & select { x = e + 1, id = .id }` has one open overload
    /// (`+`), so the checker records its site as `Choice::Hole(0)` — the same
    /// entry whatever `h` is applied to. The resolution lives at the *use*:
    /// `i = h .age` records `Def(0, 85)` (the int `+`) against the use site,
    /// and `f = h 1.5` would record the float one. So `h`'s body must be
    /// elaborated once per hole assignment, and this is where that assignment
    /// travels.
    pub holes: &'a [(usize, usize)],
    /// The definitions currently being expanded, outermost first.
    ///
    /// A definition may name itself; this tracks expansion to detect recursion.
    pub active: &'a [(usize, usize)],
}

/// The result of descending into a definition: either it is not already being
/// expanded, or this is a recursive definition.
fn enter<'a>(cx: &Ctx<'a>, module: usize, def: usize) -> R<Vec<(usize, usize)>> {
    if cx.active.contains(&(module, def)) {
        let definition = cx.input.module(module).def(def);
        let name = &definition.name;
        return Err(Error::new(format!(
            "`{name}` refers to itself; recursion is not supported"
        ))
        .at(Origin::new(module, definition.span)));
    }
    let mut next = cx.active.to_vec();
    next.push((module, def));
    Ok(next)
}

/// What elaborating one expression produced.
type R<T> = Result<T, Error>;

/// The marker a definition that is not a query fails with.
///
/// A sentinel rather than a real error: "this definition has no query to
/// elaborate" is a *finding*, not a failure, but `elaborate_module` returns one
/// `Result` per definition and a sentinel is the smallest way to add the third
/// outcome without changing that shape for every caller.
///
/// It is recognised by [`is_not_a_query`] rather than by comparing messages,
/// and its text is deliberately something no real diagnostic would say, so a
/// mistake shows up as strange output rather than as a silently swallowed
/// program error.
pub(crate) const NOT_A_QUERY: Error = Error {
    message: String::new(),
    origin: None,
    def: None,
    fault: crate::core::Fault::NotAQuery,
};

/// Whether an elaboration result means "this definition is not a query".
pub(crate) fn is_not_a_query(e: &Error) -> bool {
    e.fault == crate::core::Fault::NotAQuery
}

/// Elaborate the root module's definitions from source.
///
/// Returns one entry per definition, in source order, with the same
/// one result per root definition, in source order.
pub fn elaborate_module(input: CompilerInput<'_>, module: usize) -> Vec<(String, R<CheckedQuery>)> {
    #[cfg(test)]
    ELABORATE_RUNS.with(|runs| runs.set(runs.get() + 1));

    let mut out = Vec::new();
    for (i, d) in input.module(module).source().defs.iter().enumerate() {
        let result = validate_template_definition(module, d)
            .and_then(|()| elaborate_def(input, module, i, d));
        out.push((d.name.clone(), result));
    }
    out
}

fn validate_template_definition(module: usize, d: &ast::Def) -> R<()> {
    let ExprKind::Sql(sql) = &d.body.kind else {
        return Ok(());
    };
    let Some(ty) = d.ty.as_ref() else {
        return Ok(());
    };
    let arity = template_arity_from_type(ty, declared_kind(d));
    check_placeholders(sql, arity).map_err(|e| e.at(Origin::new(module, d.body.span)))
}

/// Elaborate one definition's body to a checked query.
fn elaborate_def(
    input: CompilerInput<'_>,
    module: usize,
    def: usize,
    d: &ast::Def,
) -> R<CheckedQuery> {
    let tc = input.type_check();
    // A rejected definition has no trustworthy types. Its checker diagnostic
    // is emitted by the compilation boundary, so it must not be elaborated.
    if tc.error_for(module, def).is_some() {
        return Err(NOT_A_QUERY);
    }
    // A definition that leaves open overloads is meaningful only at its uses:
    // it has no body of its own, so there is nothing here to elaborate. This is
    // the same situation as "not a query": it has no query body to elaborate.
    if tc.holes(module, def) > 0 {
        return Err(NOT_A_QUERY);
    }
    // A definition that is **not a query** has nothing to elaborate here, and
    // that is not a failure. `bad : expr r int -> expr r int = sql "$0"` is a
    // function; building a `CheckedQuery` from it fails with "expected a query",
    // which is true and useless. A caller must be able to tell "not a query"
    // from "a query I could not build", because the production boundary reports
    // the second as a compiler gap and must stay silent about the first.
    //
    // Structural, on the scheme, rather than by catching error text:
    // `SchemeView::Query` is `query r`, and `Function`/`Scalar`/`Row` are
    // everything else.
    if !matches!(
        tc.scheme_view(module, def),
        Some(crate::check::SchemeView::Query { .. })
    ) {
        return Err(NOT_A_QUERY);
    }
    let origin = Origin::new(module, d.span);
    let scope = input.module(module).scope();
    let active = [(module, def)];
    let env: [(String, CheckedValue); 0] = [];
    // A definition elaborated on its own has no assignment for its open
    // overloads: those are chosen per use, and a definition with holes is
    // skipped by `elaborate_module` anyway. See `Ctx::holes`.
    let holes: [(usize, usize); 0] = [];
    let cx = Ctx {
        input,
        module,
        owner: def,
        scope,
        env: &env,
        holes: &holes,
        active: &active,
    };
    let value = elaborate_query(cx, &d.body, origin)?;
    Ok(value)
}

///
/// `table "s" "t"` is the one a definition body starts from, so it is the one
/// this handles; the remaining relational primitives are reached through the
/// stage operators (`&?`, `&=`, …), and the ones that are not are reported
/// rather than silently given a made-up node.
fn elaborate_primitive(
    cx: Ctx<'_>,
    _e: &ast::Expr,
    args: &[ast::Expr],
    prim: &str,
    origin: Origin,
) -> R<CheckedQuery> {
    match (prim, args) {
        ("__table", [s, n]) => {
            let schema = string_literal(s, "table")?;
            let name = string_literal(n, "table")?;
            // The columns come from the *defining* definition's declared
            // `query {..}` type — the same place the checker read them — which
            // `owner` identifies. Without them the table has no columns, and a
            // table with unknown columns cannot be a `CheckedQuery`: every later
            // stage would have to guess, which is what `CheckedQuery::table`
            // refuses.
            let row = cx
                .input
                .type_check()
                .scheme_fields(cx.module, cx.owner)
                .ok_or_else(|| {
                    // A **program** error rather than a gap: a table whose defining
                    // definition declares no columns is genuinely untypeable, and
                    // `schema` reports exactly this. Classifying it correctly is what
                    // lets the production boundary show it to the user instead of
                    // treating it as a compiler failure — with `schema` demoted to an
                    // assertion, the wrong classification made the compiler *panic*
                    // here on a program that deserves a plain diagnostic.
                    Error::new(format!(
                    "the columns of table `{schema}.{name}` are unknown; give its definition a \
                     closed type, e.g. `t : query {{ id = int }} = table \"{schema}\" \"{name}\"`"
                ))
                    .at(origin)
                })?;
            CheckedQuery::table(schema, name, Some(crate::core::RowType::new(row)), origin)
        }
        (other, _) => Err(Error::unsupported(format!(
            "the primitive `{other}` is not elaborated here; relational primitives are reached \
             through the stage operators"
        ))
        .at(origin)),
    }
}

/// Elaborate an application of a prelude `primitive` in *expression* position.
///
/// `group .x` is the one the report example needs; `__in` is its sibling. The
/// rest are reported rather than given a made-up node.
fn elaborate_expr_primitive(
    cx: Ctx<'_>,
    prim: &str,
    args: &[ast::Expr],
    origin: Origin,
) -> R<CheckedExpr> {
    match (prim, args) {
        // `group key`: an aggregate-phase node whose type is the key's.
        ("__group", [key]) => {
            let k = elaborate_expr_inner(cx, key)?;
            CheckedExpr::group(k, origin)
        }
        // `inList [..] .x`: the list comes first, then the value, matching
        // `inList : list a -> expr r a -> expr r bool` (`prelude.cagara:110`)
        // and the order declared by the prelude. Swapping them can still
        // produce a well-typed tree, so the order is stated explicitly.
        ("__in", [list, value]) => {
            let v = elaborate_expr_inner(cx, value)?;
            let items = elaborate_exprs(cx, list_items(list))?;
            CheckedExpr::in_(v, items, false, origin)
        }
        (other, _) => Err(Error::unsupported(format!(
            "the expression primitive `{other}` is not elaborated yet"
        ))
        .at(origin)),
    }
}

/// Which join a name means, resolving the prelude's operator aliases.
///
/// `?` is `innerJoin`, `<?` is `leftJoin`, and so on
/// (`prelude.cagara:207-214`): the alias is a definition whose *body* is the
/// name it forwards to. Following that one step reads the language's own
/// declaration instead of keeping a second table here that could drift.
fn join_kind(cx: Ctx<'_>, name: &str) -> Option<crate::ir::JoinKind> {
    use crate::ir::JoinKind;
    let direct = |n: &str| match n {
        "innerJoin" => Some(JoinKind::Inner),
        "leftJoin" => Some(JoinKind::Left),
        "rightJoin" => Some(JoinKind::Right),
        "fullJoin" => Some(JoinKind::Full),
        "semiJoin" => Some(JoinKind::Semi),
        "antiJoin" => Some(JoinKind::Anti),
        _ => None,
    };
    if let Some(k) = direct(name) {
        return Some(k);
    }
    // An alias: `_<?_` forwards to `leftJoin` by a body that is just a name.
    let Some(Binding::Def(m, i)) = cx.scope.get(name).cloned() else {
        return None;
    };
    let body = &cx.input.module(m).def(i).body;
    match &body.kind {
        ExprKind::Name(target) => direct(target),
        _ => None,
    }
}

/// A string literal's value.
fn string_literal(e: &ast::Expr, what: &str) -> R<String> {
    match &e.kind {
        ExprKind::Lit(ast::Lit::Str(s)) => Ok(s.clone()),
        other => Err(Error::new(format!(
            "`{what}` expects a string, found {}",
            describe(other)
        ))
        .at(Origin::new(0, e.span))),
    }
}

/// Elaborate a query-valued expression: a table, an applied stage, or a name
/// bound to a query definition.
///
/// `owner` is the definition whose body `e` belongs to. Overload choices are
/// recorded per *definition*, keyed by the use's `ExprId`, so resolving one
/// needs the owner as well as the site — see [`choose`].
fn elaborate_query(cx: Ctx<'_>, e: &ast::Expr, origin: Origin) -> R<CheckedQuery> {
    let at = |span: Span| Origin::new(cx.module, span);
    let _ = origin;
    // A name bound by an enclosing application to a *query* argument: the `q`
    // of `f = q => users & leftJoin q (..)`. Now that the environment holds
    // values rather than expressions this is the same lookup an expression
    // binding uses — the value simply turns out to be a query.
    if let ExprKind::Name(n) = &e.kind {
        if let Some((_, CheckedValue::Query(q))) = cx.env.iter().rev().find(|(k, _)| k == n) {
            return Ok(q.clone());
        }
        // A *stage* binding: `& p` where `p` was passed `select { z = .id }`.
        // Handled by the caller, which knows the piped input; reaching here
        // means a stage is being used where a query is wanted.
        if let Some((_, CheckedValue::Stage(_))) = cx.env.iter().rev().find(|(k, _)| k == n) {
            return Err(Error::new(format!(
                "`{n}` is a stage; apply it to a query with `&` rather than using it as one"
            ))
            .at(at(e.span)));
        }
    }
    match &e.kind {
        ExprKind::Name(n) => {
            // A query-valued definition, e.g. `t` or a helper's result.
            match cx.scope.get(n) {
                Some(Binding::Def(dm, di)) => {
                    let body = &cx.input.module(*dm).def(*di).body;
                    let inner_origin = at(cx.input.module(*dm).def(*di).span);
                    // Descending into another definition runs in *its* module,
                    // owner and scope, so the context changes with it.
                    let active = enter(&cx, *dm, *di)?;
                    let inner = Ctx {
                        input: cx.input,
                        module: *dm,
                        owner: *di,
                        scope: cx.input.module(*dm).scope(),
                        active: &active,
                        holes: cx.holes,
                        env: cx.env,
                    };
                    elaborate_query(inner, body, inner_origin)
                }
                Some(Binding::Overloads(dm, is)) => {
                    let di = choose(
                        cx.input.type_check(),
                        cx.module,
                        cx.owner,
                        e.id,
                        is,
                        cx.holes,
                    )?;
                    let body = &cx.input.module(*dm).def(di).body;
                    let inner_origin = at(cx.input.module(*dm).def(di).span);
                    let active = enter(&cx, *dm, di)?;
                    let inner = Ctx {
                        input: cx.input,
                        module: *dm,
                        owner: di,
                        scope: cx.input.module(*dm).scope(),
                        active: &active,
                        holes: cx.holes,
                        env: cx.env,
                    };
                    elaborate_query(inner, body, inner_origin)
                }
                _ => Err(Error::new(format!("unknown query name `{n}`")).at(at(e.span))),
            }
        }
        // `alias.name`: a definition reached through an imported module. The
        // binding lives in the *other* module's `own` map, so both the module
        // and the scope change when descending.
        ExprKind::Proj(base, f) => {
            let ExprKind::Name(alias) = &base.kind else {
                return Err(Error::new("expected a module alias before `.`").at(at(base.span)));
            };
            let Some(Binding::Module(target)) = cx.scope.get(alias).cloned() else {
                return Err(Error::new(format!("`{alias}` is not a module")).at(at(base.span)));
            };
            let Some(binding) = cx.input.module(target).own().get(f).cloned() else {
                return Err(
                    Error::new(format!("module `{alias}` has no definition `{f}`")).at(at(e.span)),
                );
            };
            let (dm, di) = match binding {
                Binding::Def(dm, di) => (dm, di),
                Binding::Overloads(dm, is) => (
                    dm,
                    choose(
                        cx.input.type_check(),
                        cx.module,
                        cx.owner,
                        e.id,
                        &is,
                        cx.holes,
                    )?,
                ),
                _ => return Err(Error::new(format!("`{alias}.{f}` is not a query")).at(at(e.span))),
            };
            let body = cx.input.module(dm).def(di).body.clone();
            let inner_scope = cx.input.module(dm).scope().clone();

            let active = enter(&cx, dm, di)?;
            let inner = Ctx {
                input: cx.input,
                module: dm,
                owner: di,
                scope: &inner_scope,
                active: &active,
                holes: cx.holes,
                env: cx.env,
            };
            elaborate_query(inner, &body, at(e.span))
        }
        ExprKind::App(f, args) => elaborate_application(cx, e, f, args),
        other => {
            Err(Error::new(format!("expected a query, found {}", describe(other))).at(at(e.span)))
        }
    }
}

/// Elaborate an application: either a stage applied through the pipe, or a
/// definition applied to its arguments.
fn elaborate_application(
    cx: Ctx<'_>,
    e: &ast::Expr,
    f: &ast::Expr,
    args: &[ast::Expr],
) -> R<CheckedQuery> {
    let at = |span: Span| Origin::new(cx.module, span);

    // A **query-valued lambda**, e.g.
    // `f = q => users & leftJoin (q & select {..}) (.<id eq .>uid)` applied as
    // `f u`. The parameter is bound to the elaborated query argument and the
    // body is elaborated under it, with query bindings kept in `qenv`.
    //
    // This must not run for the *pipe*: `_&_ : a -> (a -> b) -> b = x => f => f x`
    // (`prelude.cagara:37`) is itself a two-parameter lambda, and every
    // `q & stage` application has exactly two arguments, so an unguarded
    // version of this branch swallows the pipe itself and dispatches `agg`,
    // `select`, … as if they were arguments of a user function. That is what
    // happened, and it is why the pipe is excluded explicitly here.
    if !matches!(&f.kind, ExprKind::Name(n) if rules::is_pipe_name(n)) {
        if let ExprKind::Name(n) = &f.kind {
            let binding = cx.scope.get(n).cloned();
            let chosen = match binding {
                Some(Binding::Def(dm, di)) => Some((dm, di)),
                Some(Binding::Overloads(dm, is)) => Some((
                    dm,
                    choose(
                        cx.input.type_check(),
                        cx.module,
                        cx.owner,
                        f.id,
                        &is,
                        cx.holes,
                    )?,
                )),
                _ => None,
            };
            if let Some((dm, di)) = chosen {
                let body = cx.input.module(dm).def(di).body.clone();
                let (mut params, mut inner) = (Vec::new(), &body);
                while let ExprKind::Lambda(p, b) = &inner.kind {
                    params.push(p.clone());
                    inner = b;
                }
                // Only when every parameter is supplied: a partially applied
                // query function has no query to return yet, and inventing one
                // would be worse than reporting. A `sql`/`primitive` body has
                // no parameters and falls through to the handling below.
                if !params.is_empty() && params.len() == args.len() {
                    // Each parameter is bound to whichever kind its argument
                    // actually is. A lambda may take a query
                    // (`f = q => users & leftJoin q (..)`) or an expression
                    // (`h = e => users & select { x = e + 1 }`), and the
                    // elaboration uses the argument's shape to decide.
                    //
                    // Trying the query reading first is deliberate: a column
                    // reference cannot be a query, and a bare name that *is* a
                    // query would elaborate either way, so the query reading is
                    // the more specific one and must win.
                    // Bind each parameter to whatever its argument *is*. One
                    // loop, one environment, because a value may be a query, an
                    // expression, a callable, or a stage — which is precisely
                    // what three parallel environments could not express, and
                    // why a partially applied function had no home.
                    let mut bound: Vec<(String, CheckedValue)> = Vec::with_capacity(args.len());
                    for (p, a) in params.iter().zip(args) {
                        bound.push((p.clone(), elaborate_value(cx, a)?));
                    }
                    let inner_scope = cx.input.module(dm).scope().clone();
                    let active = enter(&cx, dm, di)?;
                    let mut env: Vec<(String, CheckedValue)> = cx.env.to_vec();
                    env.extend(bound);
                    // The callee's open overloads are resolved by this use
                    // site, exactly as in the expression path. Setting
                    // `cx.holes` here instead was the bug that kept these
                    // definitions unelaborated: the caller's assignment
                    // addresses the *caller's* holes, not the callee's.
                    let holes = holes_at(cx, dm, di, f.id)?;
                    let callee = Ctx {
                        input: cx.input,
                        module: dm,
                        owner: di,
                        scope: &inner_scope,
                        env: &env,
                        active: &active,
                        holes: &holes,
                    };
                    return elaborate_query(callee, inner, at(e.span));
                }
            }
        }
    }

    // `table "s" "t"` and the other `__` primitives are prelude *definitions*
    // whose body is a `primitive` reference, so they arrive here as an
    // application of a name rather than as a stage. The stage table below does
    // not know them; the prelude does, so read it rather than listing names.
    if let ExprKind::Name(n) = &f.kind {
        if let Some(Binding::Def(pm, pi)) = cx.scope.get(n).cloned() {
            let body = &cx.input.module(pm).def(pi).body;
            if let ExprKind::Primitive(prim) = &body.kind {
                // A *set operation* defined as a bare primitive is still a set
                // operation, and `q = unionAll users users` is an ordinary call
                // to it. Reaching `elaborate_primitive` would report
                // "`__unionAll` is not elaborated", because that function
                // handles the primitives that build a query from nothing
                // (`table`) rather than those combining two.
                //
                // This goes to `elaborate_set_op` directly rather than through
                // `apply_stage`, because a direct call has no piped input: there
                // is nothing to pass as one, and inventing a placeholder query
                // would be a fabricated value that the constructors might accept.
                if let Some(kind) = set_kind_of(n) {
                    return elaborate_set_op(cx, kind, args, at(e.span));
                }
                return elaborate_primitive(cx, e, args, prim, at(e.span));
            }
        }
    }

    // `q & stage arg` desugars to `_&_ q (stage arg)`, and the stage operators
    // are exactly those the prelude declares at the pipe's level.
    let is_pipe = matches!(&f.kind, ExprKind::Name(n) if rules::is_pipe_name(n));
    if is_pipe && args.len() == 2 {
        let (input_e, stage_e) = (&args[0], &args[1]);
        let input = elaborate_query(cx, input_e, at(input_e.span))?;
        // A stage arrives in one of two shapes, and both are ordinary:
        //
        // * `q & select {...}` desugars to `_&_ q (_&=_ (select {...}))`, so
        //   the second argument is an application of the stage *operator*;
        // * `users &? .active` applies that operator directly, so the second
        //   argument is the stage's own argument and the operator name is
        //   already in `f`.
        //
        // The second form is what the report example's `&?`/`&*` shorthands
        // produce. Reading only the first is why `active_totals` failed with
        // "a pipeline stage must be an operator application".
        //
        // `&` itself is the exception: `q & stage` has `_&_` in `f` too, but
        // it is the *pipe*, and the stage is named by the second argument.
        let ExprKind::Name(pipe_op) = &f.kind else {
            unreachable!("`is_pipe` above checked that `f` is a name")
        };
        let is_pipe_itself = matches!(pipe_op.as_str(), "_&_" | "&");
        let (stage_name, stage_args) = if is_pipe_itself {
            match &stage_e.kind {
                // `_&_ q (_&=_ (select {...}))`: the operator is the stage.
                ExprKind::App(sf, sargs) => match &sf.kind {
                    ExprKind::Name(n) => (n.as_str(), &sargs[..]),
                    _ => {
                        return Err(Error::new("a pipeline stage must be a named operator")
                            .at(at(stage_e.span)))
                    }
                },
                // `_&_ q stageName` with no argument, e.g. `& distinct`.
                ExprKind::Name(n) => (n.as_str(), &[][..]),
                _ => {
                    return Err(Error::new("a pipeline stage must be a named operator")
                        .at(at(stage_e.span)))
                }
            }
        } else {
            // The operator in `f` *is* the stage, and `stage_e` is its argument.
            (pipe_op.as_str(), std::slice::from_ref(stage_e))
        };
        // `& p` where `p` is a *parameter* bound to a stage:
        // `h = p => users & select { id = .id } & p`, applied as
        // `a = h (select { z = .id })`. The binding holds the form the argument
        // was written in, so applying it re-runs the ordinary dispatch on that
        // form, matching the normal stage dispatch path.
        if let Some((_, CheckedValue::Stage(s))) =
            cx.env.iter().rev().find(|(k, _)| k == stage_name)
        {
            let s = s.clone();
            return apply_stage(cx, &s.op, &s.arg, input, at(input_e.span));
        }
        return apply_stage(cx, stage_name, stage_args, input, at(input_e.span));
    }

    Err(
        Error::new("only a pipeline stage applied to a query is elaborated at present")
            .at(at(e.span)),
    )
}

/// Does `op` applied to `given` arguments still await more — i.e. is this a
/// *partially applied* stage rather than a complete call?
///
/// A stage takes the piped query as its last argument, so a form written as a
/// stage supplies one fewer than the stage's arity. `select {..}` gives 1 of 2;
/// `union users users` gives 2 of 2 and is therefore a finished call, not a
/// stage to be applied later.
///
/// The arity comes from the stage definition's **signature**, counted the way
/// its type signature, so a new prelude stage needs no
/// change here. The bare prelude name and the `_op_` spelling are both looked
/// up, since either may appear.
fn stage_wants_more(cx: Ctx<'_>, op: &str, given: usize) -> bool {
    let arity = cx
        .scope
        .get(op)
        .and_then(|b| match b {
            Binding::Def(m, i) => Some((*m, *i)),
            Binding::Overloads(m, is) => is.first().map(|i| (*m, *i)),
            _ => None,
        })
        .map(|(m, i)| {
            let mut n = 0;
            let mut t = cx.input.module(m).def(i).ty.as_ref();
            while let Some(ast::TypeExpr::Fun(_, r)) = t {
                n += 1;
                t = Some(r);
            }
            n
        });
    match arity {
        // Unknown (a stage spelled some other way): assume it is a stage, which
        // is the previous behaviour and the more specific reading.
        None => true,
        Some(n) => given < n,
    }
}

/// The set operation a name denotes, for either spelling.
///
/// `&|`/`_&|_` and `union` are the same operation; recognising both here keeps
/// the two spellings from drifting apart, which is exactly the class of bug the
/// production parity check caught once already.
fn set_kind_of(name: &str) -> Option<crate::ir::SetKind> {
    match name {
        "_&|_" | "union" => Some(crate::ir::SetKind::Union),
        "_&!_" | "unionAll" => Some(crate::ir::SetKind::UnionAll),
        "_&^_" | "intersect" => Some(crate::ir::SetKind::Intersect),
        "_&~_" | "except" => Some(crate::ir::SetKind::Except),
        _ => None,
    }
}

/// A set operation written as a **direct call**: `q = unionAll users users`.
///
/// Both operands arrive in source order, and no piped query is involved. This is
/// the counterpart of the piped form (`q & unionAll other`, where the piped
/// query is the right operand) and the two must stay distinct — sharing one
/// operand order between them is what reversed a set operation earlier in this
/// work.
fn elaborate_set_op(
    cx: Ctx<'_>,
    kind: crate::ir::SetKind,
    args: &[ast::Expr],
    origin: Origin,
) -> R<CheckedQuery> {
    let at = |span: Span| Origin::new(cx.module, span);
    match args {
        [l, r] => {
            let left = elaborate_query(cx, l, at(l.span))?;
            let right = elaborate_query(cx, r, at(r.span))?;
            CheckedQuery::set(kind, left, right, origin)
        }
        _ => Err(Error::new("a set operation takes two queries").at(origin)),
    }
}

/// Is `name` something [`apply_stage`] can dispatch?
///
/// Deliberately *not* `rules::is_pipe_name`. That answers a different question
/// — whether a name is one of the `_op_` *spellings* an operator desugars to,
/// which decides where a diagnostic is located (`rules.rs:33`) — and it is
/// false for the bare prelude names. Both spellings are stages here: `_&=_` and
/// `select` are the same stage, and `is_pipe_name` recognises only the first.
///
/// Getting this wrong is not cosmetic. Using `is_pipe_name` to recognise a
/// *stage argument* fails on `select {..}`, which then falls through to the
/// scalar path and is reported as "the expression primitive `__select` is not
/// elaborated".
fn is_stage_form(name: &str) -> bool {
    matches!(
        name,
        "_&?_"
            | "where"
            | "_&=_"
            | "select"
            | "_&+_"
            | "update"
            | "_&*_"
            | "agg"
            | "_&._"
            | "order"
            | "_&-_"
            | "limit"
            | "offset"
            | "distinct"
            | "omit"
            | "prefix"
            | "suffix"
            | "_&|_"
            | "union"
            | "_&!_"
            | "unionAll"
            | "_&^_"
            | "intersect"
            | "_&~_"
            | "except"
            | "_?_"
            | "innerJoin"
            | "_<?_"
            | "leftJoin"
            | "_?>_"
            | "rightJoin"
            | "_<?>_"
            | "fullJoin"
            | "semiJoin"
            | "antiJoin"
    )
}

/// Dispatch one stage operator to its checked constructor.
///
/// The operator names come from `cagara_syntax::op_name`, so `&?` is `_&?_` —
/// the spelling the prelude defines — rather than a list maintained here.
fn apply_stage(
    cx: Ctx<'_>,
    stage_name: &str,
    stage_args: &[ast::Expr],
    input: CheckedQuery,
    origin: Origin,
) -> R<CheckedQuery> {
    let at = |span: Span| Origin::new(cx.module, span);
    let one_arg = |what: &str| -> R<&ast::Expr> {
        match stage_args {
            [a] => Ok(a),
            _ => Err(Error::new(format!("`{what}` takes one argument")).at(origin)),
        }
    };

    match stage_name {
        // The bare spelling is what `q & where p` actually desugars to:
        // `_&_ q (where p)`, where `where` is the *prelude definition*
        // (`prelude.cagara:59`). The `_&?_` operator is the shorthand that
        // forwards to it, and both are accepted so a program written either way
        // elaborates.
        "_&?_" | "where" => {
            let pred_e = one_arg("where")?;
            let pred = elaborate_expr_inner(cx, pred_e)?;
            CheckedQuery::where_(input, pred, origin)
        }
        // `select {..}`
        "_&=_" | "select" => {
            let fields_e = one_arg("select")?;
            let fields = elaborate_fields(cx, fields_e)?;
            CheckedQuery::select(input, fields, origin)
        }
        // `update {..}`
        "_&+_" | "update" => {
            let fields_e = one_arg("update")?;
            let fields = elaborate_fields(cx, fields_e)?;
            CheckedQuery::update(input, fields, origin)
        }
        // `agg {..}`
        "_&*_" | "agg" => {
            let fields_e = one_arg("agg")?;
            let fields = elaborate_fields(cx, fields_e)?;
            CheckedQuery::agg(input, fields, origin)
        }
        // `order [..]`
        "_&._" | "order" => {
            let keys_e = one_arg("order")?;
            let keys = elaborate_order(cx, keys_e)?;
            CheckedQuery::order(input, keys, origin)
        }
        // `limit n`
        "_&-_" | "limit" => {
            let n_e = one_arg("limit")?;
            let n = int_literal(n_e, "limit")?;
            CheckedQuery::limit(input, n, origin)
        }
        // `omit "k"` / `prefix "s"` / `suffix "s"`.
        "omit" => {
            let k = string_literal(one_arg("omit")?, "omit")?;
            CheckedQuery::omit(input, k, origin)
        }
        "prefix" => {
            let a = string_literal(one_arg("prefix")?, "prefix")?;
            CheckedQuery::prefix(input, a, origin)
        }
        "suffix" => {
            let a = string_literal(one_arg("suffix")?, "suffix")?;
            CheckedQuery::suffix(input, a, origin)
        }
        // `offset n` and `distinct` are plain definitions as well.
        "offset" => {
            let n = int_literal(one_arg("offset")?, "offset")?;
            CheckedQuery::offset(input, n, origin)
        }
        "distinct" => CheckedQuery::distinct(input, origin),
        // Joins: `q & innerJoin r pred`, and the operator spellings `?`, `<?`,
        // `?>`, `<?>` which the prelude defines as aliases of those names
        // (`prelude.cagara:207`). A join is not a stage operator — it sits at
        // its own, looser level — so what arrives is the name applied to
        // `[right, pred]`, with the piped query supplied last. The kind comes
        // from resolving the name through the prelude, not from a list here.
        "innerJoin" | "leftJoin" | "rightJoin" | "fullJoin" | "semiJoin" | "antiJoin" | "_?_"
        | "_<?_" | "_?>_" | "_<?>_" => {
            let kind = join_kind(cx, stage_name)
                .ok_or_else(|| Error::new(format!("`{stage_name}` is not a join")).at(origin))?;
            let (right_e, pred_e) = match stage_args {
                [r, p] => (r, p),
                _ => {
                    return Err(Error::new(format!(
                        "`{stage_name}` takes a right input and a predicate"
                    ))
                    .at(origin))
                }
            };
            let right = elaborate_query(cx, right_e, at(right_e.span))?;
            let on = elaborate_expr_inner(cx, pred_e)?;
            CheckedQuery::join(kind, input, right, on, origin)
        }
        // Set operations. The two *spellings* put the piped query on opposite
        // sides, and the difference is real rather than a quirk to smooth over.
        //
        //   * the operators `&|`, `&!`, `&^`, `&~` declare
        //     `_&|_ : query r -> query r -> query r = q => r => union q r`
        //     (`prelude.cagara:153`), so the body re-binds and the piped query
        //     `q` ends up on the **left**. The prelude's own comment says so:
        //     "Each puts the query it is piped into on the *left*, so the stage
        //     and the direct call agree — `users &~ vips` and
        //     `except users vips` are the same query."
        //   * the bare names `union`, `unionAll`, `intersect`, `except` are
        //     `query r -> query r -> query r` primitives whose first argument
        //     is the left operand, and they are declared at `&`'s own level, so
        //     `q & union other` desugars to `union other q` and the piped query
        //     lands on the **right**.
        //
        // The piped and direct forms have different operand order: for
        // `s & select {..} & union (users & select {..})` the result is
        // `users UNION s`, while `union A B` emits `A UNION B`. Treating both
        // spellings alike reverses whichever one is wrong — which is what the
        // production parity check caught, and why `except` and `union` cannot
        // both be served by one operand order.
        "_&|_" | "_&!_" | "_&^_" | "_&~_" => {
            let other_e = one_arg("a set operation")?;
            let other = elaborate_query(cx, other_e, at(other_e.span))?;
            let kind = match stage_name {
                "_&|_" => crate::ir::SetKind::Union,
                "_&!_" => crate::ir::SetKind::UnionAll,
                "_&^_" => crate::ir::SetKind::Intersect,
                _ => crate::ir::SetKind::Except,
            };
            // Piped query on the left, the operator's argument on the right.
            CheckedQuery::set(kind, input, other, origin)
        }
        "union" | "unionAll" | "intersect" | "except" => {
            // A bare-name set operation reached through the pipe: `q & union o`
            // desugars to `union o q`, so the single argument is the *left*
            // operand and the piped query is the right one. The direct-call
            // shape (`union a b`) has no piped query and is handled by
            // `elaborate_set_op`, which is why the argument count decides.
            let kind = set_kind_of(stage_name).expect("matched by name just above");
            match stage_args {
                [only] => {
                    let other = elaborate_query(cx, only, at(only.span))?;
                    CheckedQuery::set(kind, other, input, origin)
                }
                _ => elaborate_set_op(cx, kind, stage_args, origin),
            }
        }
        // A **user-defined stage**: `no_id = omit "id"` then `users & no_id`.
        //
        // `no_id`'s body is `omit "id"`, which is a *function* of one query
        // (`omit` is `string -> query r -> query k`), so applying it to the
        // piped input follows its declared function shape. Recognising it here means
        // elaborating the body with the input bound to whatever the body's
        // argument is, which is what `elaborate_stage_body` does.
        _ => {
            if let Some(Binding::Def(dm, di)) = cx.scope.get(stage_name).cloned() {
                return elaborate_user_stage(cx, dm, di, stage_args, input, origin);
            }
            Err(Error::unsupported(format!(
                "`{stage_name}` applied to a query as a stage is not elaborated; it is not a \
                 prelude stage and not a definition that takes a query"
            ))
            .at(origin))
        }
    }
}

/// Elaborate a user-defined stage: a definition whose body is a query function
/// applied to its argument, e.g. `no_id = omit "id"`.
///
/// The body is an application (`omit "id"`) whose *last* argument is the query
/// it takes, so the piped input is bound there. Only supported source shapes
/// are handled; anything else is reported rather than given a
/// guessed tree.
fn elaborate_user_stage(
    cx: Ctx<'_>,
    dm: usize,
    di: usize,
    stage_args: &[ast::Expr],
    input: CheckedQuery,
    origin: Origin,
) -> R<CheckedQuery> {
    let body = cx.input.module(dm).def(di).body.clone();
    let name = cx.input.module(dm).def(di).name.clone();
    let inner_scope = cx.input.module(dm).scope().clone();
    let active = enter(&cx, dm, di)?;
    let inner = Ctx {
        input: cx.input,
        module: dm,
        owner: di,
        scope: &inner_scope,
        active: &active,
        holes: cx.holes,
        env: cx.env,
    };
    // A stage name applied to no further argument: the body already *is* the
    // function, so the input is its only argument. `no_id = omit "id"` reaches
    // here, and so does `vips = except vips`-style composition.
    if stage_args.is_empty() {
        return apply_stage_body(inner, &body, input, origin);
    }
    Err(Error::new(format!(
        "the user-defined stage `{name}` takes {} argument(s); only a stage applied directly to \
         the piped query is elaborated",
        stage_args.len()
    ))
    .at(origin))
}

/// Apply a definition's body to a query, treating the body as a one-argument
/// query function.
fn apply_stage_body(
    cx: Ctx<'_>,
    body: &ast::Expr,
    input: CheckedQuery,
    origin: Origin,
) -> R<CheckedQuery> {
    let ExprKind::App(f, args) = &body.kind else {
        return Err(Error::new(format!(
            "expected a stage definition's body to apply a stage to its argument, found {}",
            describe(&body.kind)
        ))
        .at(origin));
    };
    let ExprKind::Name(stage) = &f.kind else {
        return Err(Error::new("a stage definition must apply a named stage").at(origin));
    };
    apply_stage(cx, stage, args, input, origin)
}

/// Elaborate a record of fields (`{ a = .., b = .. }`).
fn elaborate_fields(cx: Ctx<'_>, e: &ast::Expr) -> R<Vec<(String, CheckedExpr)>> {
    match &e.kind {
        ExprKind::Record(fs) => fs
            .iter()
            .map(|(n, x)| Ok((n.clone(), elaborate_expr_inner(cx, x)?)))
            .collect(),
        // `select {.a, .b}`: the parser produces a record of projections.
        other => Err(Error::new(format!(
            "expected a record of fields, found {}",
            describe(other)
        ))
        .at(Origin::new(cx.module, e.span))),
    }
}

/// Elaborate an `order [...]` key list.
fn elaborate_order(cx: Ctx<'_>, e: &ast::Expr) -> R<Vec<(CheckedExpr, bool)>> {
    let at = |span: Span| Origin::new(cx.module, span);
    let ExprKind::List(xs) = &e.kind else {
        return Err(Error::new(format!(
            "expected a list of sort keys, found {}",
            describe(&e.kind)
        ))
        .at(at(e.span)));
    };
    xs.iter()
        .map(|x| {
            // `asc e` / `desc e` are applications of the prelude's `asc`/`desc`.
            if let ExprKind::App(f, args) = &x.kind {
                if let ExprKind::Name(n) = &f.kind {
                    if let [inner] = &args[..] {
                        let asc = match n.as_str() {
                            "asc" => true,
                            "desc" => false,
                            _ => return Ok((elaborate_expr_inner(cx, x)?, true)),
                        };
                        return Ok((elaborate_expr_inner(cx, inner)?, asc));
                    }
                }
            }
            Ok((elaborate_expr_inner(cx, x)?, true))
        })
        .collect()
}

/// Elaborate a scalar expression to a `CheckedExpr`.
///
/// This is the module's public entry point for scalar elaboration.
pub fn elaborate_expr(
    input: CompilerInput<'_>,
    module: usize,
    owner: usize,
    scope: &std::collections::HashMap<String, Binding>,
    e: &ast::Expr,
) -> R<CheckedExpr> {
    // No enclosing definition, so the cycle guard starts empty: a definition
    // that names itself is caught when *it* is descended into, which keeps this
    // the same caller-facing signature it has always had.
    let active: [(usize, usize); 0] = [];
    // No enclosing lambda either, so the environment starts empty; this is the
    // caller-facing entry point and keeps the signature it has always had.
    let env: [(String, CheckedValue); 0] = [];
    let holes: [(usize, usize); 0] = [];
    elaborate_expr_inner(
        Ctx {
            input,
            module,
            owner,
            scope,
            env: &env,
            holes: &holes,
            active: &active,
        },
        e,
    )
}

/// Elaborate a scalar expression to a `CheckedExpr`.
///
/// The type comes from the checker ([`TypeCheck::use_ty`]); this function never
/// invents one. When the checker recorded no scalar for the node, that is
/// reported rather than guessed — a `CheckedExpr` with a made-up `ScalarType`
/// would be worse than an error, because everything downstream trusts it.
fn elaborate_expr_inner(cx: Ctx<'_>, e: &ast::Expr) -> R<CheckedExpr> {
    let _at = |span: Span| Origin::new(cx.module, span);
    let origin = Origin::new(cx.module, e.span);
    let ty = |what: &str| -> R<ScalarType> {
        cx.input
            .type_check()
            .use_ty(cx.module, e.id)
            .ok_or_else(|| {
                Error::new(format!(
                    "the checker recorded no scalar type for this {what}, so it cannot be \
                 elaborated without inventing one"
                ))
                .at(origin)
            })
    };

    match &e.kind {
        // `.x`, `.<x`, `.>x`
        ExprKind::Field(side, n) => {
            Ok(CheckedExpr::column(*side, n.clone(), ty("column")?, origin))
        }
        ExprKind::Lit(l) => Ok(CheckedExpr::lit(lit_of(l), origin)),
        // A `sql "..."` template used as an expression.
        ExprKind::Sql(sql) => {
            let t = ty("template")?;
            CheckedExpr::template(sql.clone(), vec![], t, origin)
        }
        ExprKind::List(xs) => {
            // A list is not a scalar expression; `in` handles lists.
            let _ = xs;
            Err(Error::new("a list is not a scalar expression").at(origin))
        }
        // A name bound by an enclosing application — a lambda parameter — wins
        // over the module scope. The binding is the *already elaborated*
        // argument, so this substitutes
        // a value rather than re-deriving one.
        ExprKind::Name(n) if cx.env.iter().any(|(k, _)| k == n) => {
            let (_, v) = cx
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == n)
                .expect("checked just above");
            match v {
                // The binding is an expression: it *is* the value.
                CheckedValue::Expr(e) => Ok(e.clone()),
                // The binding is a callable used without arguments. That is
                // legal only when it is not what this position wants; report
                // rather than emit a partially applied function as an
                // expression, which would be a fabricated `CheckedExpr`.
                CheckedValue::Callable(_) => Err(Error::new(format!(
                    "`{n}` is a function here; apply it to its argument(s)"
                ))
                .at(origin)),
                CheckedValue::Query(_) | CheckedValue::Stage(_) => Err(Error::new(format!(
                    "`{n}` is not a scalar expression in this position"
                ))
                .at(origin)),
            }
        }
        // A bare name is a *nullary* definition: `count` is
        // `agg (expr r int) = sql "COUNT(*)"`, with no arguments, so it
        // appears as a name rather than an application. Anything with
        // arguments goes through `elaborate_call` below.
        ExprKind::Name(n) => match cx.scope.get(n).cloned() {
            Some(Binding::Def(dm, di)) => {
                let body = &cx.input.module(dm).def(di).body;
                if let ExprKind::Sql(sql) = &body.kind {
                    let (phase, ty) =
                        cx.input.type_check().result_expr(dm, di).ok_or_else(|| {
                            Error::new(format!("`{n}` does not return an expression")).at(origin)
                        })?;
                    match phase {
                        Phase::Const | Phase::Row => {
                            CheckedExpr::template(sql.clone(), vec![], ty, origin)
                        }
                        Phase::Agg => CheckedExpr::agg_template(sql.clone(), vec![], ty, origin),
                        Phase::Win => Err(Error::unsupported(format!(
                            "`{n}` is a window function; window specs are not elaborated yet"
                        ))
                        .at(origin)),
                    }
                } else {
                    // A definition whose body is an *expression*:
                    // `adult = .age >= 18`, used bare as in `users & where adult`.
                    // Its body is elaborated in the definition's own module and
                    // scope, which is where its names resolve.
                    let body = body.clone();
                    let inner_scope = cx.input.module(dm).scope().clone();
                    let active = enter(&cx, dm, di)?;
                    let inner = Ctx {
                        input: cx.input,
                        module: dm,
                        owner: di,
                        scope: &inner_scope,
                        env: cx.env,
                        active: &active,
                        holes: cx.holes,
                    };
                    elaborate_expr_inner(inner, &body)
                }
            }
            _ => Err(Error::new(format!(
                "`{n}` is not a scalar expression this layer can elaborate"
            ))
            .at(origin)),
        },
        ExprKind::App(f, args) => elaborate_call(cx, f, args, origin),
        other => Err(Error::unsupported(format!(
            "cannot elaborate {} as a scalar expression",
            describe(other)
        ))
        .at(origin)),
    }
}

/// Elaborate an application in expression position: `upper .name`,
/// `coalesce 0.0 .x`, `sum .amount`, and so on.
fn elaborate_call(
    cx: Ctx<'_>,
    f: &ast::Expr,
    args: &[ast::Expr],
    origin: Origin,
) -> R<CheckedExpr> {
    let ExprKind::Name(name) = &f.kind else {
        return Err(
            Error::new("only a named definition can be applied in an expression").at(origin),
        );
    };

    // A name bound by an enclosing application is applied through the value it
    // holds, before any definition of the same name is considered. This is what
    // makes `g (f x)` inside `_>>>_`'s body work: `g` and `f` are not definitions
    // there, they are the composition's parameters.
    if let Some((_, v)) = cx.env.iter().rev().find(|(k, _)| k == name).cloned() {
        let mut value = v;
        for a in args {
            let arg = elaborate_value(cx, a)?;
            value = match apply_value(cx, value, arg, origin)? {
                Applied::Done(v) | Applied::Partial(v) => v,
            };
        }
        return match value {
            CheckedValue::Expr(e) => Ok(e),
            CheckedValue::Callable(_) => {
                Err(Error::new(format!("`{name}` is applied to too few arguments")).at(origin))
            }
            CheckedValue::Query(_) | CheckedValue::Stage(_) => Err(Error::unsupported(format!(
                "`{name}` does not denote an expression"
            ))
            .at(origin)),
        };
    }

    // A `sql "..."` template definition: its body is the template and its
    // argument count comes from its signature.
    let binding = cx.scope.get(name).cloned();
    let (dm, di) = match binding {
        Some(Binding::Def(dm, di)) => (dm, di),
        Some(Binding::Overloads(dm, is)) => (
            dm,
            choose(
                cx.input.type_check(),
                cx.module,
                cx.owner,
                f.id,
                &is,
                cx.holes,
            )?,
        ),
        _ => {
            return Err(Error::new(format!(
                "`{name}` is not a scalar function this layer can elaborate"
            ))
            .at(origin))
        }
    };

    let body = &cx.input.module(dm).def(di).body;
    // `group .x` and the other expression primitives are prelude definitions
    // whose body is a `primitive` reference, exactly like `table`. The prelude
    // names them, so read it rather than listing them here.
    if let ExprKind::Primitive(prim) = &body.kind {
        return elaborate_expr_primitive(cx, prim, args, origin);
    }
    // An **ordinary function body**: `f : expr r int -> expr r int = x => ...`.
    // Bind each parameter to the elaborated argument and elaborate the body
    // under that environment. Nothing is re-derived: the
    // argument's type came from the checker and the body's structure is the
    // user's.
    if matches!(body.kind, ExprKind::Lambda(..)) {
        return elaborate_lambda(cx, (dm, di), name, body, args, f.id, origin);
    }
    // A body that is an **application**: `nextWeek = addDays 7 >>> truncWeek`,
    // which desugars to `_>>>_ (addDays 7) truncWeek`.
    //
    // The body is elaborated into a value — here a callable, because `_>>>_` is
    // a lambda and `addDays 7` is a partially applied template — and this call's
    // arguments are then applied to it through the same `apply_value` every
    // other application uses.
    //
    // No case for `>>>` or `<<<` appears here and none is needed: nothing in
    // this path looks at the operator's name, so a user's own composition
    // operator goes through it unchanged.
    if matches!(body.kind, ExprKind::App(..)) {
        let body = body.clone();
        let inner_scope = cx.input.module(dm).scope().clone();
        let active = enter(&cx, dm, di)?;
        let holes = holes_at(cx, dm, di, f.id)?;
        let callee = Ctx {
            input: cx.input,
            module: dm,
            owner: di,
            scope: &inner_scope,
            env: cx.env,
            holes: &holes,
            active: &active,
        };
        let mut value = elaborate_value(callee, &body)?;
        for a in args {
            let arg = elaborate_value(cx, a)?;
            value = match apply_value(cx, value, arg, origin)? {
                Applied::Done(v) | Applied::Partial(v) => v,
            };
        }
        return match value {
            CheckedValue::Expr(e) => Ok(e),
            CheckedValue::Callable(_) => Err(Error::new(format!(
                "`{name}` is applied to too few arguments; a partially applied function is not \
                 an expression"
            ))
            .at(origin)),
            CheckedValue::Query(_) | CheckedValue::Stage(_) => Err(Error::unsupported(format!(
                "`{name}` does not denote an expression"
            ))
            .at(origin)),
        };
    }
    let ExprKind::Sql(sql) = &body.kind else {
        return Err(Error::unsupported(format!(
            "`{name}` is neither a `sql` template nor a function; this layer cannot elaborate \
             its body"
        ))
        .at(origin));
    };

    // Which constructor builds this is a property of the definition's
    // *signature*: `expr r a -> expr r bool` is a scalar template, while
    // `sum : expr r int -> agg (expr r (maybe int))` returns an aggregate, so
    // its call is an `agg_template`. Read it from the same signature the
    // checker used rather than guessing from the name.
    //
    // This comes *before* elaborating the arguments, because a window call's
    // first argument is a spec record rather than an expression: elaborating
    // them uniformly first would reject `rowNumber spec`.
    //
    // Only the scalar type is taken from the resolved scheme. The *phase* is
    // deliberately not: the scheme is what the checker inferred, and for a
    // template whose arguments promote it that disagrees with the node built
    // below.
    let (_phase, ty) = cx.input.type_check().result_expr(dm, di).ok_or_else(|| {
        Error::new(format!(
            "`{name}` does not return an expression, so its call has no phase or scalar type"
        ))
        .at(origin)
    })?;

    // Which constructor builds this is a property of how the result is
    // **declared**, not of the scheme the checker resolved.
    //
    // The two disagree, and the disagreement matters.
    // `inc : agg (expr r int) -> expr r int = sql "$1 + 1"` declares a scalar
    // result, and for inference the checker rewrites its scheme to
    // `agg (expr r int) -> agg (expr r int)`. Reading that resolved scheme says
    // "aggregate", so an earlier version built an `agg_template` — but `inc` is
    // a scalar template whose *argument* happens to be an aggregate. Wrapping
    // it in `agg_template` instead trips the no-nested-aggregates rule.
    //
    // `CheckedExpr::template` folds the argument phases with `rules::mix`, which
    // is exactly the promotion rule used by checked templates,
    // so a scalar template over aggregate arguments still comes out aggregate —
    // without the constructors fighting each other.
    let declared = declared_kind(cx.input.module(dm).def(di));
    match declared {
        // Declared `expr ..`: a scalar template. Its phase comes from its
        // arguments (`CheckedExpr::template` folds them with `rules::mix`), so
        // `inc count` is an aggregate even though `inc` declares a scalar
        // result.
        DeclaredKind::Scalar => {
            let inner = elaborate_exprs(cx, args)?;
            CheckedExpr::template(sql.clone(), inner, ty, origin)
        }
        // Declared `agg (..)`: an aggregate template. `sum` is this shape, and
        // its arguments must be row phase — the depth-1 rule, which
        // `agg_template` checks. Routing this through `template` instead loses
        // the aggregate phase and makes enclosing `agg` stages reject a column
        // as ungrouped.
        DeclaredKind::Agg => {
            let inner = elaborate_exprs(cx, args)?;
            CheckedExpr::agg_template(sql.clone(), inner, ty, origin)
        }
        // A window template's *first* argument is its spec (`winspec r ->
        // expr r a -> win (expr r a)`); the rest are its value arguments. The
        // spec is built separately because it is a record, not an expression.
        DeclaredKind::Win => {
            let (spec_e, value_args) = args.split_first().ok_or_else(|| {
                Error::new(format!("`{name}` is a window function and needs a spec")).at(origin)
            })?;
            let spec = elaborate_winspec(cx, spec_e)?;
            let values = elaborate_exprs(cx, value_args)?;
            CheckedExpr::win_template(sql.clone(), values, spec, ty, origin)
        }
    }
}

/// What a name bound by an enclosing application stands for.
///
/// A value during source expansion. It may be partially applied and awaiting
/// more arguments, or a stage awaiting its input query.
#[derive(Debug, Clone)]
pub(crate) enum CheckedValue {
    /// A query. The stage constructs produce these.
    Query(CheckedQuery),
    /// A scalar/aggregate/window expression.
    Expr(CheckedExpr),
    /// A callable, awaiting arguments.
    ///
    /// One variant for every callable, whether the callee was a
    /// closure, a template, or a primitive. Splitting them would reintroduce the
    /// special-casing this type exists to remove.
    Callable(Box<Callable>),
    /// A *stage*: a function from a query to a query, written as an operator
    /// applied to its own argument.
    ///
    /// `a = h (select { z = .id })` passes one and `& p` applies it.
    Stage(Box<CheckedStage>),
}

/// A callable value: enough to apply it, and to know when it is complete.
#[derive(Debug, Clone)]
pub(crate) enum Callable {
    /// A `sql` template and the arguments supplied so far.
    Template {
        sql: String,
        /// Expression arguments the whole call takes, so the value is complete
        /// when `args.len() == arity`.
        arity: usize,
        args: Vec<CheckedExpr>,
        ty: ScalarType,
        kind: DeclaredKind,
    },
    /// A user-written function: a lambda body plus the binders still unbound.
    ///
    /// The body is held as source so applying it re-runs `elaborate_lambda`'s
    /// binding logic, which is what keeps closures, shadowing and the hole
    /// assignment behaving exactly as they do on a direct call.
    Closure {
        /// Where the body lives: the module and definition it belongs to.
        def: (usize, usize),
        /// Binders not yet supplied, outermost first.
        params: Vec<String>,
        /// The body under those binders.
        body: ast::Expr,
        /// The name it was reached by, for diagnostics.
        name: String,
        /// The `ExprId` of the application that produced this value: the use
        /// site the checker keyed this instance's overloads by.
        site: u32,
        /// The bindings this closure captured, innermost last.
        ///
        /// A partially applied function carries the arguments already bound to
        /// it, so the next application can see them — a curried
        /// next application would not see them — a curried `k = x => y => x`
        /// applied one argument at a time would lose `x`.
        env: Vec<(String, CheckedValue)>,
    },
}

/// A stage passed as an argument, kept in the form it was written.
#[derive(Debug, Clone)]
pub(crate) struct CheckedStage {
    /// The stage operator: `_&=_`, `select`, `_&?_`, …
    pub op: String,
    /// The stage's own argument, as written. Elaborated against the input row
    /// when the stage is applied.
    pub arg: Vec<ast::Expr>,
}

/// How a `sql` template's result is *declared* to behave, read from its type
/// expression.
///
/// Deliberately not the resolved scheme — see the comment at the use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclaredKind {
    Scalar,
    Agg,
    Win,
}

/// The declared result kind of a `sql` template definition.
/// How many expression arguments a `sql` template definition takes.
///
/// Counted from its signature,
/// minus one for a window template, whose first argument is its spec rather than
/// an expression.
fn template_arity(cx: Ctx<'_>, dm: usize, di: usize) -> usize {
    let d = cx.input.module(dm).def(di);
    template_arity_from_type(
        d.ty.as_ref().expect("typed template definition"),
        declared_kind(d),
    )
}

fn template_arity_from_type(t: &ast::TypeExpr, kind: DeclaredKind) -> usize {
    let mut n: usize = 0;
    let mut ty = t;
    while let ast::TypeExpr::Fun(_, r) = ty {
        n += 1;
        ty = r;
    }
    if kind == DeclaredKind::Win {
        n.saturating_sub(1)
    } else {
        n
    }
}

fn check_placeholders(sql: &str, expr_args: usize) -> R<()> {
    let bytes = sql.as_bytes();
    let mut seen = vec![false; expr_args + 1];
    let mut i = 0;
    let mut highest = 0;
    let mut in_string = false;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if in_string && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
            } else {
                in_string = !in_string;
                i += 1;
            }
            continue;
        }
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == i + 1 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: a `$` at byte {start} is not a placeholder"
            )));
        }
        let n = sql[i + 1..end]
            .parse::<usize>()
            .map_err(|_| Error::new("SQL template placeholder is too large"))?;
        let glued = |byte: u8| byte == b'_' || byte.is_ascii_alphanumeric() || byte == b'$';
        if in_string || bytes.get(end).copied().is_some_and(glued) || (i > 0 && glued(bytes[i - 1]))
        {
            return Err(Error::new(format!(
                "SQL template `{sql}`: `{}` must stand alone (not inside a name or string)",
                &sql[i..end]
            )));
        }
        if n == 0 {
            return Err(Error::new(format!(
                "SQL template `{sql}`: placeholders are 1-based, so `${n}` is not an argument"
            )));
        }
        if n > expr_args {
            return Err(Error::new(format!(
                "template uses placeholders up to ${n} but its signature has {expr_args} expression argument(s)"
            )));
        }
        seen[n] = true;
        highest = highest.max(n);
        i = end;
    }
    if let Some(missing) = (1..=highest).find(|&n| !seen[n]) {
        return Err(Error::new(format!(
            "SQL template `{sql}` is missing placeholder `${missing}`"
        )));
    }
    if highest < expr_args {
        return Err(Error::new(format!(
            "template uses placeholders up to ${highest} but its signature has {expr_args} expression argument(s)"
        )));
    }
    Ok(())
}

fn declared_kind(d: &ast::Def) -> DeclaredKind {
    let Some(mut t) = d.ty.as_ref() else {
        return DeclaredKind::Scalar;
    };
    // Strip the argument arrows to reach the result.
    while let ast::TypeExpr::Fun(_, r) = t {
        t = r;
    }
    match t {
        ast::TypeExpr::App { head, .. } if head == "agg" => DeclaredKind::Agg,
        ast::TypeExpr::App { head, .. } if head == "win" => DeclaredKind::Win,
        _ => DeclaredKind::Scalar,
    }
}

/// The overload candidates chosen for `callee` **used at `site`**.
///
/// This makes a polymorphic definition elaborate differently at each use.
///
/// `tc.holes(dm, di)` counts the callee's open overloads. `tc.choice(dm, di,
/// site, k)` then holds the answer recorded *for that use*: the checker
/// rewrites a definition's own `Hole(k)` constraints into `Site(site, k)` when
/// it instantiates that definition at a use (`infer.rs:835`), and records the
/// chosen candidate against the callee keyed by that use site.
///
/// An open hole with nothing recorded is a real gap, not an absence of
/// overloads, so it is reported rather than defaulting to candidate 0.
fn holes_at(cx: Ctx<'_>, dm: usize, di: usize, site: u32) -> R<Vec<(usize, usize)>> {
    let n = cx.input.type_check().holes(dm, di);
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        // Read through the **caller's** map, not the callee's.
        //
        // The checker records a definition's body once, leaving its overloads
        // as `Hole(k)`; the resolution performed when that definition is
        // *instantiated at a use* is recorded against the definition being
        // checked at the time, which is the caller
        // (`infer.rs:1746`, `self.active.last()`). So for `ok = h .age` the
        // candidate for `h`'s hole lives in `ok`'s entry at `ok`'s site, and
        // `tc.choice(h, use_site, k)` is `None`.
        //
        // Verified rather than assumed: `h`'s own entry holds
        // `[(7, 0, Hole(0))]` while `ok`'s holds `[(16, 0, Def(0, 85))]`.
        match cx.input.type_check().choice(cx.module, cx.owner, site, k) {
            Some(Choice::Def(_, chosen)) => out.push((dm, chosen)),
            // The use sits at a site that is itself an open hole **of the
            // definition being elaborated** — the nested case:
            //
            //   twice = x => x + x
            //   quad  = x => twice (twice x)
            //   q     = orders & select { a = quad .user_id }
            //
            // `quad` has two holes (its two uses of `twice`), recorded as
            // `Hole(0)`/`Hole(1)`; `q` resolves both against `quad`'s entry at
            // its own site. So when elaborating `quad`'s body the assignment is
            // already in `cx.holes`, indexed by exactly the `k` the site
            // recorded.
            Some(Choice::Hole(h)) => match cx.holes.get(h) {
                Some((_, chosen)) => out.push((dm, *chosen)),
                None => {
                    let name = &cx.input.module(dm).def(di).name;
                    return Err(Error::new(format!(
                        "`{name}` is used at an open hole ({h}) of the definition being \
                         elaborated, and no assignment for it was supplied"
                    )));
                }
            },
            None => {
                let name = &cx.input.module(dm).def(di).name;
                return Err(Error::new(format!(
                    "`{name}` has an open overload (hole {k}) that this use does not \
                     instantiate; give the definition a type signature that fixes it"
                )));
            }
        }
    }
    Ok(out)
}

/// Elaborate an application of an ordinary function body, by binding the
/// parameters to the elaborated arguments and elaborating the body.
///
/// Bind parameters in an environment, then elaborate the body. This preserves
/// shadowing and lets nested applications refer to captured values.
///
/// Curried parameters are handled one application at a time: `k = x => y => x`
/// applied to one argument binds `x` and leaves a
/// function over `y`, so the extra `Lambda` layers are bound as more arguments
/// arrive. Applying fewer arguments than binders is not an error here — the
/// result is simply elaborated again on the next application.
fn elaborate_lambda(
    cx: Ctx<'_>,
    // The callee: `(module, definition)`. Bundled because they always travel
    // together as "which definition is this", and separately they pushed this
    // function past the argument-count lint for no benefit.
    callee_def: (usize, usize),
    name: &str,
    body: &ast::Expr,
    args: &[ast::Expr],
    // The `ExprId` of the application: the *use site* the checker keyed this
    // instance's overload choices by. A line comment because a doc comment is
    // not allowed on a parameter.
    site: u32,
    origin: Origin,
) -> R<CheckedExpr> {
    let (dm, di) = callee_def;
    // Walk the nested `Lambda`s, binding one argument to each parameter.
    let (mut params, mut inner) = (Vec::new(), body);
    while let ExprKind::Lambda(p, b) = &inner.kind {
        params.push(p.clone());
        inner = b;
    }
    if args.len() > params.len() {
        return Err(Error::new(format!(
            "`{name}` takes {} argument(s) but {} were given",
            params.len(),
            args.len()
        ))
        .at(origin));
    }

    // Elaborate the arguments in the *caller's* context — they are expressions
    // the caller wrote, so they resolve against the caller's environment, not
    // the callee's. Each becomes whatever it denotes, so a parameter can be
    // bound to a query, an expression, a callable, or a stage.
    let mut bound: Vec<(String, CheckedValue)> = Vec::with_capacity(args.len());
    for (p, a) in params.iter().zip(args) {
        bound.push((p.clone(), elaborate_value(cx, a)?));
    }

    let scope = cx.input.module(dm).scope().clone();
    let active = enter(&cx, dm, di)?;
    // The callee's environment is the caller's bindings plus the new ones.
    // Keeping the caller's bindings lets a nested lambda see what the outer
    // application bound.
    let mut env: Vec<(String, CheckedValue)> = cx.env.to_vec();
    let base = env.len();
    env.extend(bound);
    // The callee's own open overloads are resolved by *this* use site, which is
    // what makes `h .age` and `h 1.5` elaborate to different trees from one
    // definition. Replacing rather than extending is deliberate: a hole index is
    // local to the definition being elaborated, so the caller's assignment
    // would be addressing different holes.
    let holes = holes_at(cx, dm, di, site)?;
    let callee = Ctx {
        input: cx.input,
        module: dm,
        owner: di,
        scope: &scope,
        env: &env,
        active: &active,
        holes: &holes,
    };

    // No arguments left over: the body itself is the result. This is the
    // `f .age` case where the body is fully applied.
    if args.len() == params.len() {
        return elaborate_expr_inner(callee, inner);
    }

    // Fewer arguments than binders: the body is still a function. Elaborating
    // it as an expression is not meaningful, so report rather than invent a
    // node — a partial application used as a scalar has no `CheckedExpr` that
    // would be true.
    let _ = base;
    Err(Error::new(format!(
        "`{name}` needs {} more argument(s); a partially applied function is not an expression",
        params.len() - args.len()
    ))
    .at(origin))
}

// ── elaborating an argument into a value ────────────────────────────────────

/// Elaborate an expression in *argument* position into whatever it denotes.
///
/// This is the piece the earlier design was missing. It replaces a sequence of
/// "try it as a stage, then as a query, then as an expression" guesses with a
/// single reading driven by the **shape of the expression**.
///
/// Order matters where two readings could both succeed:
///
/// * a **stage operator applied to fewer arguments than it takes** is a stage —
///   `select {..}` supplies one of two, with the query still to come. A stage
///   operator applied to *all* its arguments (`unionAll users users`) is a
///   finished call and must be read as a query instead;
/// * a **partially applied template or lambda** is a callable, checked before
///   the query reading, because such a thing is not a query;
/// * otherwise a query, then an expression.
///
/// A function that is already a value in the environment is returned as-is.
fn elaborate_value(cx: Ctx<'_>, e: &ast::Expr) -> R<CheckedValue> {
    let at = |span: Span| Origin::new(cx.module, span);

    // A name, or a bare call, may already be bound to a value.
    if let ExprKind::Name(n) = &e.kind {
        if let Some((_, v)) = cx.env.iter().rev().find(|(k, _)| k == n) {
            let v = v.clone();
            // A bound expression or query is the value; a bound *callable* is
            // applied to nothing here, so it is passed along unchanged — that
            // is how `f` in `h f` forwards a function.
            return Ok(v);
        }
    }

    // A stage: an operator of the pipeline's level supplied with fewer
    // arguments than it takes.
    if let ExprKind::App(sf, sargs) = &e.kind {
        if let ExprKind::Name(op) = &sf.kind {
            if is_stage_form(op) && stage_wants_more(cx, op, sargs.len()) {
                return Ok(CheckedValue::Stage(Box::new(CheckedStage {
                    op: op.clone(),
                    arg: sargs.to_vec(),
                })));
            }
        }
    }

    // A callable: a partially applied template or a lambda with binders left.
    if let Some(c) = elaborate_callable(cx, e)? {
        return Ok(CheckedValue::Callable(Box::new(c)));
    }

    // A query, when the expression denotes one.
    if let Ok(q) = elaborate_query(cx, e, at(e.span)) {
        return Ok(CheckedValue::Query(q));
    }

    // Otherwise an expression.
    Ok(CheckedValue::Expr(elaborate_expr_inner(cx, e)?))
}

/// Elaborate an expression that denotes a *function* into a callable, if it is
/// one. `Ok(None)` when it is not, so the caller can try other readings.
fn elaborate_callable(cx: Ctx<'_>, e: &ast::Expr) -> R<Option<Callable>> {
    let (f, args): (&ast::Expr, &[ast::Expr]) = match &e.kind {
        ExprKind::App(f, args) => (f, args),
        ExprKind::Name(_) => (e, &[]),
        _ => return Ok(None),
    };
    let ExprKind::Name(name) = &f.kind else {
        return Ok(None);
    };
    let Some(binding) = cx.scope.get(name).cloned() else {
        return Ok(None);
    };
    let (dm, di) = match binding {
        Binding::Def(dm, di) => (dm, di),
        Binding::Overloads(dm, is) => {
            match choose(
                cx.input.type_check(),
                cx.module,
                cx.owner,
                f.id,
                &is,
                cx.holes,
            ) {
                Ok(di) => (dm, di),
                Err(_) => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let body = cx.input.module(dm).def(di).body.clone();
    match &body.kind {
        // A `sql` template still short of arguments.
        ExprKind::Sql(sql) => {
            let arity = template_arity(cx, dm, di);
            check_placeholders(sql, arity).map_err(|e| e.at(Origin::new(dm, body.span)))?;
            if args.len() >= arity {
                return Ok(None);
            }
            let Some((_, ty)) = cx.input.type_check().result_expr(dm, di) else {
                return Ok(None);
            };
            Ok(Some(Callable::Template {
                sql: sql.clone(),
                arity,
                args: elaborate_exprs(cx, args)?,
                ty,
                kind: declared_kind(cx.input.module(dm).def(di)),
            }))
        }
        // A lambda with binders left over.
        ExprKind::Lambda(..) => {
            let (mut params, mut inner) = (Vec::new(), &body);
            while let ExprKind::Lambda(p, b) = &inner.kind {
                params.push(p.clone());
                inner = b;
            }
            if args.len() >= params.len() {
                return Ok(None);
            }
            // The supplied arguments are bound immediately; the rest wait.
            let mut env: Vec<(String, CheckedValue)> = Vec::new();
            for (p, a) in params.iter().zip(args) {
                env.push((p.clone(), elaborate_value(cx, a)?));
            }
            let supplied = args.len();
            params.drain(..supplied);
            let body = inner.clone();
            Ok(Some(Callable::Closure {
                def: (dm, di),
                params,
                body,
                name: name.clone(),
                site: f.id,
                env,
            }))
        }
        _ => Ok(None),
    }
}

// ── applying a value to an argument ─────────────────────────────────────────

/// The outcome of applying a value to one argument.
///
/// Applying one argument either completes the call or returns a partially
/// applied callable.
enum Applied {
    /// The call completed; this is its value.
    Done(CheckedValue),
    /// Still a function, with this argument recorded.
    Partial(CheckedValue),
}

/// Apply one argument to a value, completing it or staying deferred.
fn apply_value(cx: Ctx<'_>, f: CheckedValue, arg: CheckedValue, origin: Origin) -> R<Applied> {
    let CheckedValue::Callable(c) = f else {
        return Err(Error::unsupported("cannot apply this to an argument").at(origin));
    };
    match *c {
        Callable::Template {
            sql,
            arity,
            mut args,
            ty,
            kind,
        } => {
            // A template's operands are expressions; a query cannot be one.
            let CheckedValue::Expr(e) = arg else {
                return Err(Error::new(format!(
                    "`{sql}` takes an expression, but a query was supplied"
                ))
                .at(origin));
            };
            args.push(e);
            if args.len() < arity {
                return Ok(Applied::Partial(CheckedValue::Callable(Box::new(
                    Callable::Template {
                        sql,
                        arity,
                        args,
                        ty,
                        kind,
                    },
                ))));
            }
            // Complete. The constructor follows the declared kind, the same
            // choice a direct call makes, so a composed call and a written-out
            // one cannot differ.
            let built = complete_template(cx, kind, sql, args, ty, origin)?;
            Ok(Applied::Done(CheckedValue::Expr(built)))
        }
        Callable::Closure {
            def,
            mut params,
            body,
            name,
            site,
            env: captured,
        } => {
            let Some(param) = params.first().cloned() else {
                return Err(Error::new(format!("`{name}` takes no more arguments")).at(origin));
            };
            params.remove(0);
            // The captured bindings come first so a nested closure still sees
            // what an outer application bound, as `Closure::env` does.
            let mut env = captured;
            env.push((param, arg));
            if params.is_empty() {
                let (dm, di) = def;
                let scope = cx.input.module(dm).scope().clone();
                let holes = holes_at(cx, dm, di, site)?;
                let mut active = cx.active.to_vec();
                if !active.contains(&(dm, di)) {
                    active.push((dm, di));
                }
                let callee = Ctx {
                    input: cx.input,
                    module: dm,
                    owner: di,
                    scope: &scope,
                    env: &env,
                    holes: &holes,
                    active: &active,
                };
                let v = elaborate_expr_inner(callee, &body)?;
                return Ok(Applied::Done(CheckedValue::Expr(v)));
            }
            Ok(Applied::Partial(CheckedValue::Callable(Box::new(
                Callable::Closure {
                    def,
                    params,
                    body,
                    name,
                    site,
                    env,
                },
            ))))
        }
    }
}

/// Build a completed template call from its collected arguments.
fn complete_template(
    cx: Ctx<'_>,
    kind: DeclaredKind,
    sql: String,
    args: Vec<CheckedExpr>,
    ty: ScalarType,
    origin: Origin,
) -> R<CheckedExpr> {
    match kind {
        // A scalar template folds its arguments' phases (`rules::mix`), which is
        // what makes `inc count` an aggregate even though `inc` declares a
        // scalar result.
        DeclaredKind::Scalar => CheckedExpr::template(sql, args, ty, origin),
        // An aggregate template must **not** go through `template`. Its phase is
        // `Agg` because of what it *is*, not because of what its arguments are,
        // and `agg_template` additionally enforces the depth-1 rule (an
        // aggregate's arguments are row phase). Routing it through `template`
        // silently built a row-phase node, so `apply sum` applied to a column
        // reported "uses a column that is not grouped" where a direct
        // `sum .amount` produced `SUM(amount)`.
        //
        // These are the same two constructors the direct call in
        // `elaborate_call` uses, so a deferred call and a written-out one cannot
        // disagree about phase — which is what the parity check would otherwise
        // report, or worse, not report.
        DeclaredKind::Agg => CheckedExpr::agg_template(sql, args, ty, origin),
        DeclaredKind::Win => {
            // A window template's first argument is its spec, which was
            // elaborated as a `WinSpecChecked`, not an expression. A composed
            // window call would need the spec carried in the value; reported
            // rather than guessed.
            let _ = cx;
            Err(Error::unsupported(
                "a composed window function is not elaborated; call it directly instead",
            )
            .at(origin))
        }
    }
}

/// Elaborate a list of argument expressions.
fn elaborate_exprs(cx: Ctx<'_>, args: &[ast::Expr]) -> R<Vec<CheckedExpr>> {
    args.iter().map(|a| elaborate_expr_inner(cx, a)).collect()
}

/// Elaborate a window spec: a record literal, or a name bound to one.
///
/// `spec = { partition = [.user_id], order = [desc .created_at] }` is an
/// ordinary definition whose body is a record, and `runningFrame` is one whose
/// body is a call of the prelude's `rows`. Both are read here rather than
/// demanded inline.
fn elaborate_winspec(cx: Ctx<'_>, e: &ast::Expr) -> R<crate::checked::WinSpecChecked> {
    let origin = Origin::new(cx.module, e.span);
    // A name bound to a spec, resolved to its body.
    if let ExprKind::Name(n) = &e.kind {
        if let Some(Binding::Def(dm, di)) = cx.scope.get(n).cloned() {
            let body = cx.input.module(dm).def(di).body.clone();
            // The body is evaluated in its own module's scope.
            let inner_scope = cx.input.module(dm).scope().clone();

            let active = enter(&cx, dm, di)?;
            let inner = Ctx {
                input: cx.input,
                module: dm,
                owner: di,
                scope: &inner_scope,
                active: &active,
                holes: cx.holes,
                env: cx.env,
            };
            return elaborate_winspec(inner, &body);
        }
    }
    let ExprKind::Record(fs) = &e.kind else {
        return Err(Error::new(format!(
            "a window spec must be a record `{{ partition = .., order = .., frame = .. }}`, \
             found {}",
            describe(&e.kind)
        ))
        .at(origin));
    };

    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    for (k, v) in fs {
        match k.as_str() {
            rules::winspec::PARTITION => {
                partition = list_items(v)
                    .iter()
                    .map(|x| elaborate_expr_inner(cx, x))
                    .collect::<R<_>>()?;
            }
            rules::winspec::ORDER => {
                order = elaborate_order(cx, v)?;
            }
            rules::winspec::FRAME => {
                frame = Some(elaborate_frame(cx, v)?);
            }
            other => {
                return Err(Error::new(format!(
                    "unknown window spec field `{other}`; expected {}",
                    rules::winspec::names()
                ))
                .at(origin))
            }
        }
    }
    crate::checked::window_spec(partition, order, frame)
}

/// The elements of a list literal, or an empty slice for anything else.
fn list_items(e: &ast::Expr) -> &[ast::Expr] {
    match &e.kind {
        ExprKind::List(xs) => xs,
        _ => &[],
    }
}

/// Elaborate a frame: `rows a b`, or a name bound to one.
fn elaborate_frame(cx: Ctx<'_>, e: &ast::Expr) -> R<crate::ir::Frame> {
    let origin = Origin::new(cx.module, e.span);
    if let ExprKind::Name(n) = &e.kind {
        if let Some(Binding::Def(dm, di)) = cx.scope.get(n).cloned() {
            let body = cx.input.module(dm).def(di).body.clone();
            let inner_scope = cx.input.module(dm).scope().clone();

            let active = enter(&cx, dm, di)?;
            let inner = Ctx {
                input: cx.input,
                module: dm,
                owner: di,
                scope: &inner_scope,
                active: &active,
                holes: cx.holes,
                env: cx.env,
            };
            return elaborate_frame(inner, &body);
        }
    }
    // `rows start end`.
    if let ExprKind::App(f, args) = &e.kind {
        if matches!(&f.kind, ExprKind::Name(n) if n == "rows") {
            if let [a, b] = &args[..] {
                let start = elaborate_bound(cx, a)?;
                let end = elaborate_bound(cx, b)?;
                return crate::checked::frame(start, end);
            }
        }
    }
    Err(Error::new("a frame must be `rows start end`").at(origin))
}

/// Elaborate a frame bound: `unboundedPreceding`, `currentRow`,
/// `preceding n`, `following n`.
fn elaborate_bound(cx: Ctx<'_>, e: &ast::Expr) -> R<crate::ir::Bound> {
    use crate::ir::Bound;
    let origin = Origin::new(cx.module, e.span);
    let name = match &e.kind {
        ExprKind::Name(n) => n.as_str(),
        // `preceding n` / `following n`.
        ExprKind::App(f, args) => {
            let ExprKind::Name(n) = &f.kind else {
                return Err(Error::new("a frame bound must be a named value").at(origin));
            };
            let [n_e] = &args[..] else {
                return Err(Error::new(format!("`{n}` takes one argument")).at(origin));
            };
            let count = int_literal(n_e, n)?;
            return match n.as_str() {
                "preceding" => Ok(Bound::Preceding(count)),
                "following" => Ok(Bound::Following(count)),
                _ => {
                    // A definition bound to a bound value.
                    let _ = cx;
                    Err(Error::new(format!("`{n}` is not a frame bound")).at(origin))
                }
            };
        }
        _ => return Err(Error::new("a frame bound must be a named value").at(origin)),
    };
    match name {
        "unboundedPreceding" => Ok(Bound::UnboundedPreceding),
        "unboundedFollowing" => Ok(Bound::UnboundedFollowing),
        "currentRow" => Ok(Bound::CurrentRow),
        other => Err(Error::new(format!("`{other}` is not a frame bound")).at(origin)),
    }
}

/// Which overload candidate a use resolved to.
///
/// The checker records choices per *definition*, keyed by the `ExprId` of the
/// use inside that definition's body (`overload::record`, reachable as
/// `TypeCheck::choices_of(module, def)`). So the definition whose body contains
/// the use is what this needs — not the candidate list's module, which is a
/// different thing that an earlier draft conflated.
fn choose(
    tc: &TypeCheck,
    module: usize,
    owner_def: usize,
    site: u32,
    cands: &[usize],
    holes: &[(usize, usize)],
) -> R<usize> {
    if cands.len() == 1 {
        return Ok(cands[0]);
    }
    for (choice_site, _hole, c) in tc.choices_of(module, owner_def) {
        if choice_site != site {
            continue;
        }
        match c {
            Choice::Def(_, di) => return Ok(di),
            // The site is open inside its own definition — `Hole(k)` — so the
            // candidate for this instance comes from the hole assignment the
            // caller passed down. This is the per-use instantiation: the same
            // site resolves differently for `h .age` and `h 1.5`, and the
            // assignment is the only thing that distinguishes them.
            Choice::Hole(k) => {
                if let Some((_, di)) = holes.get(k) {
                    return Ok(*di);
                }
                return Err(Error::new(format!(
                    "hole {k} of this use was not instantiated (definition {owner_def}, site \
                     {site}); the caller did not supply an assignment"
                )));
            }
        }
    }
    Err(Error::new(format!(
        "the checker recorded no overload choice for this use (definition {owner_def}, site \
         {site}); it has {} candidate(s)",
        cands.len()
    )))
}

/// The core `Lit` for a syntax literal.
fn lit_of(l: &ast::Lit) -> Lit {
    match l {
        ast::Lit::Int(i) => Lit::Int(*i),
        ast::Lit::Float(s) => Lit::Float(s.clone()),
        ast::Lit::Str(s) => Lit::Str(s.clone()),
        ast::Lit::Bool(b) => Lit::Bool(*b),
    }
}

/// A whole-number literal's value, for `limit`/`offset`.
fn int_literal(e: &ast::Expr, what: &str) -> R<i64> {
    match &e.kind {
        ExprKind::Lit(ast::Lit::Int(n)) if *n >= 0 => Ok(*n),
        ExprKind::Lit(ast::Lit::Int(n)) => Err(Error::new(format!(
            "`{what}` needs a non-negative count, got {n}"
        ))
        .at(Origin::new(0, e.span))),
        other => Err(Error::new(format!(
            "`{what}` expects an int, found {}",
            describe(other)
        ))
        .at(Origin::new(0, e.span))),
    }
}

/// A short description of an expression form, for error messages.
fn describe(k: &ExprKind) -> &'static str {
    match k {
        ExprKind::Name(_) => "a name",
        ExprKind::Lit(_) => "a literal",
        ExprKind::Field(..) => "a column reference",
        ExprKind::Proj(..) => "a projection",
        ExprKind::App(..) => "an application",
        ExprKind::Lambda(..) => "a function",
        ExprKind::Record(_) => "a record",
        ExprKind::List(_) => "a list",
        ExprKind::Sql(_) => "a `sql` template",
        ExprKind::Primitive(_) => "a `primitive` reference",
        ExprKind::Error => "a syntax error",
    }
}
