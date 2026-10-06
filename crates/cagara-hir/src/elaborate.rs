//! Source elaboration: build the checked tree directly from source.
//!
//! This is the layer that makes the checked core *authoritative* rather than a
//! parallel description. Before it, the only way to obtain a `CheckedQuery`
//! was to rebuild one from the evaluator's own `Rel` output
//! (`from_rel_unchecked`), so a test could compare the two and pass even if
//! nothing in the compiler ever constructed a `CheckedQuery` from source.
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
//!   operator is a pipeline stage, so stage structure comes from the same table
//!   the evaluator uses rather than from a hand-written list here.
//!
//! Re-deriving any of those would create a second source of truth for types,
//! which is the failure this whole migration exists to remove. See
//! `verify/elaborator-feasibility.md` for the probe results that established
//! each fact is available.
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
use crate::core::{Error, Origin, ScalarType};
use crate::ir::{Lit, Phase};
use crate::rules;
use crate::workspace::{Binding, Workspace};
use cagara_syntax::ast::{self, ExprKind, Span};

/// What every elaboration step needs: the workspace, the type check, the
/// module and definition being elaborated, and the scope names resolve in.
///
/// These five travelled as five separate parameters through every function
/// here, which is what `clippy`'s `too_many_arguments` was pointing at. They
/// are one thing — "where in the program are we, and what do we know" — so
/// they are one value.
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub ws: &'a Workspace,
    pub tc: &'a TypeCheck,
    /// The module the expression being elaborated lives in.
    pub module: usize,
    /// The definition whose body it belongs to. Overload choices are recorded
    /// per definition, keyed by the use's `ExprId`, so resolving one needs the
    /// owner as well as the site.
    pub owner: usize,
    pub scope: &'a std::collections::HashMap<String, Binding>,
}

/// What elaborating one expression produced.
type R<T> = Result<T, Error>;

/// Elaborate the root module's definitions from source.
///
/// Returns one entry per definition, in source order, with the same
/// `Elaborated` outcomes the evaluator-based path reports — so a caller can
/// switch between them and compare.
pub fn elaborate_module(
    ws: &Workspace,
    tc: &TypeCheck,
    module: usize,
) -> Vec<(String, R<CheckedQuery>)> {
    let mut out = Vec::new();
    for (i, d) in ws.modules[module].module.defs.iter().enumerate() {
        let result = elaborate_def(ws, tc, module, i, d);
        out.push((d.name.clone(), result));
    }
    out
}

/// Elaborate one definition's body to a checked query.
fn elaborate_def(
    ws: &Workspace,
    tc: &TypeCheck,
    module: usize,
    def: usize,
    d: &ast::Def,
) -> R<CheckedQuery> {
    // A definition the checker rejected has no trustworthy types, so there is
    // nothing to elaborate: report the checker's own failure rather than build
    // a tree out of unsolved variables.
    if let Some(e) = tc.error_for(module, def) {
        return Err(Error::new(e.message.clone()));
    }
    // A definition that leaves open overloads is meaningful only at its uses.
    if tc.holes(module, def) > 0 {
        return Err(Error::new(format!(
            "`{}` leaves open overloads; it has no body of its own",
            d.name
        )));
    }
    let origin = Origin::new(module, d.span);
    let scope = &ws.modules[module].scope;
    let cx = Ctx { ws, tc, module, owner: def, scope };
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
            let row = cx.tc.scheme_fields(cx.module, cx.owner).ok_or_else(|| {
                Error::new(format!(
                    "the columns of table `{schema}.{name}` are unknown; give its definition a \
                     closed type, e.g. `t : query {{ id = int }} = table \"{schema}\" \"{name}\"`"
                ))
                .at(origin)
            })?;
            CheckedQuery::table(schema, name, Some(crate::core::RowType::new(row)), origin)
        }
        (other, _) => Err(Error::new(format!(
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
        (other, _) => Err(Error::new(format!(
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
    let body = &cx.ws.modules[m].module.defs[i].body;
    match &body.kind {
        ExprKind::Name(target) => direct(target),
        _ => None,
    }
}

/// A string literal's value.
fn string_literal(e: &ast::Expr, what: &str) -> R<String> {
    match &e.kind {
        ExprKind::Lit(ast::Lit::Str(s)) => Ok(s.clone()),
        other => Err(Error::new(format!("`{what}` expects a string, found {}", describe(other)))
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
    match &e.kind {
        ExprKind::Name(n) => {
            // A query-valued definition, e.g. `t` or a helper's result.
            match cx.scope.get(n) {
                Some(Binding::Def(dm, di)) => {
                    let body = &cx.ws.modules[*dm].module.defs[*di].body;
                    let inner_origin = at(cx.ws.modules[*dm].module.defs[*di].span);
                    // Descending into another definition runs in *its* module,
                    // owner and scope, so the context changes with it.
                    let inner = Ctx {
                        ws: cx.ws,
                        tc: cx.tc,
                        module: *dm,
                        owner: *di,
                        scope: &cx.ws.modules[*dm].scope,
                    };
                    elaborate_query(inner, body, inner_origin)
                }
                Some(Binding::Overloads(dm, is)) => {
                    let di = choose(cx.tc, cx.module, cx.owner, e.id, is)?;
                    let body = &cx.ws.modules[*dm].module.defs[di].body;
                    let inner_origin = at(cx.ws.modules[*dm].module.defs[di].span);
                    let inner = Ctx {
                        ws: cx.ws,
                        tc: cx.tc,
                        module: *dm,
                        owner: di,
                        scope: &cx.ws.modules[*dm].scope,
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
            let Some(binding) = cx.ws.modules[target].own.get(f).cloned() else {
                return Err(Error::new(format!("module `{alias}` has no definition `{f}`"))
                    .at(at(e.span)));
            };
            let (dm, di) = match binding {
                Binding::Def(dm, di) => (dm, di),
                Binding::Overloads(dm, is) => {
                    (dm, choose(cx.tc, cx.module, cx.owner, e.id, &is)?)
                }
                _ => {
                    return Err(
                        Error::new(format!("`{alias}.{f}` is not a query")).at(at(e.span))
                    )
                }
            };
            let body = cx.ws.modules[dm].module.defs[di].body.clone();
            let inner_scope = cx.ws.modules[dm].scope.clone();
            let inner = Ctx { ws: cx.ws, tc: cx.tc, module: dm, owner: di, scope: &inner_scope };
            elaborate_query(inner, &body, at(e.span))
        }
        ExprKind::App(f, args) => elaborate_application(cx, e, f, args),
        other => Err(Error::new(format!(
            "expected a query, found {}",
            describe(other)
        ))
        .at(at(e.span))),
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

    // `table "s" "t"` and the other `__` primitives are prelude *definitions*
    // whose body is a `primitive` reference, so they arrive here as an
    // application of a name rather than as a stage. The stage table below does
    // not know them; the prelude does, so read it rather than listing names.
    if let ExprKind::Name(n) = &f.kind {
        if let Some(Binding::Def(pm, pi)) = cx.scope.get(n).cloned() {
            let body = &cx.ws.modules[pm].module.defs[pi].body;
            if let ExprKind::Primitive(prim) = &body.kind {
                return elaborate_primitive(cx, e, args, prim, at(e.span));
            }
        }
    }

    // `q & stage arg` desugars to `_&_ q (stage arg)`, and the stage operators
    // are exactly those the prelude declares at the pipe's level — the same
    // table the evaluator consults, so the two cannot disagree about what a
    // stage is.
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
        return apply_stage(cx, stage_name, stage_args, input, at(input_e.span));
    }

    Err(Error::new(
        "only a pipeline stage applied to a query is elaborated at present",
    )
    .at(at(e.span)))
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
            let kind = join_kind(cx, stage_name).ok_or_else(|| {
                Error::new(format!("`{stage_name}` is not a join")).at(origin)
            })?;
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
        // The set-operation stages take the other query as their argument.
        "_&|_" | "union" | "_&!_" | "unionAll" | "_&^_" | "intersect" | "_&~_" | "except" => {
            let other_e = one_arg("a set operation")?;
            let other = elaborate_query(cx, other_e, at(other_e.span))?;
            let kind = match stage_name {
                "_&|_" | "union" => crate::ir::SetKind::Union,
                "_&!_" | "unionAll" => crate::ir::SetKind::UnionAll,
                "_&^_" | "intersect" => crate::ir::SetKind::Intersect,
                _ => crate::ir::SetKind::Except,
            };
            CheckedQuery::set(kind, input, other, origin)
        }
        other => Err(Error::new(format!(
            "the stage `{other}` is not elaborated yet; its spelling comes from the prelude's \
             declarations and this layer does not yet handle it"
        ))
        .at(origin)),
    }
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
        return Err(Error::new(format!("expected a list of sort keys, found {}", describe(&e.kind)))
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
/// This is the module's public entry point: it keeps the five separate
/// parameters its callers pass and builds the [`Ctx`] the helpers below share,
/// so the public signature is the only place those five appear together.
pub fn elaborate_expr(
    ws: &Workspace,
    tc: &TypeCheck,
    module: usize,
    owner: usize,
    scope: &std::collections::HashMap<String, Binding>,
    e: &ast::Expr,
) -> R<CheckedExpr> {
    elaborate_expr_inner(Ctx { ws, tc, module, owner, scope }, e)
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
        cx.tc.use_ty(cx.module, e.id).ok_or_else(|| {
            Error::new(format!(
                "the checker recorded no scalar type for this {what}, so it cannot be \
                 elaborated without inventing one"
            ))
            .at(origin)
        })
    };

    match &e.kind {
        // `.x`, `.<x`, `.>x`
        ExprKind::Field(side, n) => Ok(CheckedExpr::column(*side, n.clone(), ty("column")?, origin)),
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
        // A bare name is a *nullary* definition: `count` is
        // `agg (expr r int) = sql "COUNT(*)"`, with no arguments, so it
        // appears as a name rather than an application. Anything with
        // arguments goes through `elaborate_call` below.
        ExprKind::Name(n) => match cx.scope.get(n).cloned() {
            Some(Binding::Def(dm, di)) => {
                let body = &cx.ws.modules[dm].module.defs[di].body;
                if let ExprKind::Sql(sql) = &body.kind {
                    let (phase, ty) = cx.tc.result_expr(dm, di).ok_or_else(|| {
                        Error::new(format!("`{n}` does not return an expression")).at(origin)
                    })?;
                    match phase {
                        Phase::Const | Phase::Row => {
                            CheckedExpr::template(sql.clone(), vec![], ty, origin)
                        }
                        Phase::Agg => CheckedExpr::agg_template(sql.clone(), vec![], ty, origin),
                        Phase::Win => Err(Error::new(format!(
                            "`{n}` is a window function; window specs are not elaborated yet"
                        ))
                        .at(origin)),
                    }
                } else {
                    Err(Error::new(format!(
                        "`{n}` is defined by an expression, not a `sql` template; ordinary \
                         definitions used bare are not elaborated yet"
                    ))
                    .at(origin))
                }
            }
            _ => Err(Error::new(format!(
                "`{n}` is not a scalar expression this layer can elaborate"
            ))
            .at(origin)),
        },
        ExprKind::App(f, args) => elaborate_call(cx, f, args, origin),
        other => Err(Error::new(format!(
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
            Error::new("only a named definition can be applied in an expression").at(origin)
        );
    };

    // A `sql "..."` template definition: its body is the template and its
    // argument count comes from its signature.
    let binding = cx.scope.get(name).cloned();
    let (dm, di) = match binding {
        Some(Binding::Def(dm, di)) => (dm, di),
        Some(Binding::Overloads(dm, is)) => (dm, choose(cx.tc, cx.module, cx.owner, f.id, &is)?),
        _ => {
            return Err(Error::new(format!(
                "`{name}` is not a scalar function this layer can elaborate"
            ))
            .at(origin))
        }
    };

    let body = &cx.ws.modules[dm].module.defs[di].body;
    // `group .x` and the other expression primitives are prelude definitions
    // whose body is a `primitive` reference, exactly like `table`. The prelude
    // names them, so read it rather than listing them here.
    if let ExprKind::Primitive(prim) = &body.kind {
        return elaborate_expr_primitive(cx, prim, args, origin);
    }
    let ExprKind::Sql(sql) = &body.kind else {
        return Err(Error::new(format!(
            "`{name}` is not a `sql` template; ordinary function bodies are not elaborated yet"
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
    let (phase, ty) = cx.tc.result_expr(dm, di).ok_or_else(|| {
        Error::new(format!(
            "`{name}` does not return an expression, so its call has no phase or scalar type"
        ))
        .at(origin)
    })?;

    match phase {
        Phase::Const | Phase::Row => {
            let inner = elaborate_exprs(cx, args)?;
            CheckedExpr::template(sql.clone(), inner, ty, origin)
        }
        Phase::Agg => {
            let inner = elaborate_exprs(cx, args)?;
            CheckedExpr::agg_template(sql.clone(), inner, ty, origin)
        }
        // A window template's *first* argument is its spec (`winspec r ->
        // expr r a -> win (expr r a)`); the rest are the value arguments. The
        // spec is built separately because it is a record, not an expression.
        Phase::Win => {
            let (spec_e, value_args) = args.split_first().ok_or_else(|| {
                Error::new(format!("`{name}` is a window function and needs a spec")).at(origin)
            })?;
            let spec = elaborate_winspec(cx, spec_e)?;
            let values = elaborate_exprs(cx, value_args)?;
            CheckedExpr::win_template(sql.clone(), values, spec, ty, origin)
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
            let body = cx.ws.modules[dm].module.defs[di].body.clone();
            // The body is evaluated in its own module's scope.
            let inner_scope = cx.ws.modules[dm].scope.clone();
            let inner = Ctx { ws: cx.ws, tc: cx.tc, module: dm, owner: di, scope: &inner_scope };
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
            let body = cx.ws.modules[dm].module.defs[di].body.clone();
            let inner_scope = cx.ws.modules[dm].scope.clone();
            let inner = Ctx { ws: cx.ws, tc: cx.tc, module: dm, owner: di, scope: &inner_scope };
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
fn choose(tc: &TypeCheck, module: usize, owner_def: usize, site: u32, cands: &[usize]) -> R<usize> {
    if cands.len() == 1 {
        return Ok(cands[0]);
    }
    for (choice_site, _hole, c) in tc.choices_of(module, owner_def) {
        if choice_site != site {
            continue;
        }
        if let Choice::Def(_, di) = c {
            return Ok(di);
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
        other => Err(Error::new(format!("`{what}` expects an int, found {}", describe(other)))
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
