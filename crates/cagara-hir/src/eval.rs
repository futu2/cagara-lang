//! Compile-time evaluator. Runs a Cagara program (prelude included) and
//! reduces each definition to a value; query definitions become `CoreTerm`,
//! which is erased to `Rel` IR at the one boundary (`root_queries_checked`)
//! where the SQL backend and the schema validator need it.
//!
//! The evaluator is still the production path. What changed is what flows
//! through it: a `Prim` no longer *is* the meaning of `where` — it is
//! classified (`Prim::classify`) and handed to an explicit `CoreTerm`
//! constructor, so the intermediate representation carries the relational
//! structure and `Rel` is only an encoding of it.

use crate::check::{Choice, TypeCheck};
use crate::core_term::CoreTerm;
use crate::ir::{Lit, Loc, Rel};
use crate::prims::{self, build_tpl};
use crate::value::{err, Closure, EResult, Env, EvalError, Inst, Template, TplKind, Value};
use crate::workspace::{Binding, Diag, Workspace};
use cagara_syntax::ast::{self, ExprKind, Span, TypeExpr};
use std::collections::HashMap;
use std::rc::Rc;

/// Evaluation follows the type checker's overload choices: each definition
/// is evaluated per assignment of its overload holes.
pub struct Evaluator<'w> {
    ws: &'w Workspace,
    tc: &'w TypeCheck,
    cache: HashMap<Inst, Value>,
    active: Vec<(usize, usize)>,
    /// Application depth, so a self-applying closure (`f = x => f x`) is
    /// rejected instead of recursing until the stack overflows.
    depth: usize,
    /// How many definitions are being evaluated below the one asked for.
    /// `active` catches cycles, but a long chain of acyclic references
    /// (`a1 = f`, `a2 = a1`, …) still costs frames per link.
    def_depth: usize,
}

/// Deepest application nesting. `active` already catches recursion between
/// definitions; this catches a definition that applies *itself* as a value
/// (`f = x => f x`), which is not a definition cycle but still never ends.
/// Legitimate code reaches it too (every `&` stage applies a prelude
/// closure), so the diagnostic talks about nesting, not non-termination.
const MAX_DEPTH: usize = 256;

/// Deepest nesting of definition references. An evaluator runs on threads as
/// small as 2 MiB (the language server), and in a debug build each level
/// costs around nine frames across `inst_value` / `eval_def` / `eval`, so
/// this stays well under it — 300 nested references overflow such a thread.
const MAX_DEF_NESTING: usize = 64;

/// Attach a location to an error that does not have one yet.
fn at<T>(r: EResult<T>, module: usize, span: Span) -> EResult<T> {
    r.map_err(|mut e| {
        if e.span.is_none() {
            e.module = module;
            e.span = Some(span);
        }
        e
    })
}

impl<'w> Evaluator<'w> {
    pub fn new(ws: &'w Workspace, tc: &'w TypeCheck) -> Self {
        Evaluator {
            ws,
            tc,
            cache: HashMap::new(),
            active: Vec::new(),
            depth: 0,
            def_depth: 0,
        }
    }

    /// Value of a definition with no open overloads.
    pub fn def_value(&mut self, m: usize, i: usize) -> EResult<Value> {
        self.inst_value(Inst {
            def: (m, i),
            holes: vec![],
        })
    }

    fn inst_value(&mut self, inst: Inst) -> EResult<Value> {
        if let Some(v) = self.cache.get(&inst) {
            return Ok(v.clone());
        }
        let ws = self.ws;
        let (m, i) = inst.def;
        let def = &ws.modules[m].module.defs[i];
        if self.active.contains(&(m, i)) {
            let msg = format!(
                "`{}` refers to itself; recursion is not supported",
                def.name
            );
            return at(err(msg), m, def.span);
        }
        if self.def_depth >= MAX_DEF_NESTING {
            let msg = format!(
                "`{}` is used through a chain of more than {MAX_DEF_NESTING} definitions; \
                 define a name before the definitions that use it",
                def.name
            );
            return at(err(msg), m, def.span);
        }
        self.def_depth += 1;
        self.active.push((m, i));
        let r = self.eval_def(m, def, Rc::new(inst.clone()));
        self.def_depth -= 1;
        self.active.pop();
        let v = r?;
        self.cache.insert(inst, v.clone());
        Ok(v)
    }

    fn eval_def(&mut self, m: usize, def: &ast::Def, inst: Rc<Inst>) -> EResult<Value> {
        let v = match &def.body.kind {
            ExprKind::Sql(sql) => at(template_def(sql, def.ty.as_ref()), m, def.body.span)?,
            ExprKind::Primitive(name) => {
                at(primitive_def(name, def.ty.as_ref()), m, def.body.span)?
            }
            _ => self.eval(m, &inst, &Env::default(), &def.body)?,
        };
        Ok(attach_schema(v, def.ty.as_ref()))
    }

    pub fn eval(&mut self, m: usize, inst: &Rc<Inst>, env: &Env, e: &ast::Expr) -> EResult<Value> {
        at(self.eval_inner(m, inst, env, e), m, e.span)
    }

    /// Candidate chosen for hole `k` at `site` in the current instance.
    fn choose(&self, inst: &Inst, site: u32, k: usize) -> EResult<(usize, usize)> {
        let (m, i) = inst.def;
        match self.tc.choice(m, i, site, k) {
            Some(Choice::Def(dm, di)) => Ok((dm, di)),
            Some(Choice::Hole(h)) if h < inst.holes.len() => Ok(inst.holes[h]),
            _ => err("this overloaded name could not be resolved (see the type errors)"),
        }
    }

    /// A definition used at `site`, with its holes filled from the choices.
    fn use_def(&mut self, inst: &Inst, site: u32, dm: usize, di: usize) -> EResult<Value> {
        let holes = (0..self.tc.holes(dm, di))
            .map(|k| self.choose(inst, site, k))
            .collect::<EResult<_>>()?;
        self.inst_value(Inst {
            def: (dm, di),
            holes,
        })
    }

    fn use_binding(&mut self, inst: &Inst, site: u32, b: Binding, n: &str) -> EResult<Value> {
        match b {
            Binding::Def(dm, di) => self.use_def(inst, site, dm, di),
            Binding::Overloads(..) => {
                let (dm, di) = self.choose(inst, site, 0)?;
                self.use_def(inst, site, dm, di)
            }
            Binding::Prim(p) if p.arity() == 0 => prims::call(p, vec![]),
            Binding::Prim(p) => Ok(Value::Prim(p, vec![])),
            Binding::Module(_) => err(format!(
                "`{n}` is a module; refer to a definition as `{n}.name`"
            )),
        }
    }

    fn eval_inner(
        &mut self,
        m: usize,
        inst: &Rc<Inst>,
        env: &Env,
        e: &ast::Expr,
    ) -> EResult<Value> {
        match &e.kind {
            ExprKind::Name(n) => match env.get(n) {
                Some(v) => Ok(v.clone()),
                None => match self.ws.modules[m].scope.get(n).cloned() {
                    Some(b) => self.use_binding(inst, e.id, b, n),
                    None => err(format!("unknown name `{n}`")),
                },
            },
            ExprKind::Lit(l) => Ok(Value::Lit(match l {
                ast::Lit::Int(i) => Lit::Int(*i),
                ast::Lit::Float(s) => Lit::Float(s.clone()),
                ast::Lit::Str(s) => Lit::Str(s.clone()),
                ast::Lit::Bool(b) => Lit::Bool(*b),
            })),
            ExprKind::Field(side, n) => Ok(Value::Expr(Box::new(CoreTerm::col(*side, n.clone())))),
            ExprKind::Proj(base, f) => {
                if let ExprKind::Name(n) = &base.kind {
                    let ws = self.ws;
                    if env.get(n).is_none() {
                        if let Some(Binding::Module(target)) = ws.modules[m].scope.get(n) {
                            return match ws.modules[*target].own.get(f).cloned() {
                                Some(b) => self.use_binding(inst, e.id, b, f),
                                None => err(format!(
                                    "module `{}` has no definition `{f}`",
                                    ws.modules[*target].path.display()
                                )),
                            };
                        }
                    }
                }
                match self.eval(m, inst, env, base)? {
                    Value::Record(fs) => match fs.into_iter().find(|(k, _)| k == f) {
                        Some((_, v)) => Ok(v),
                        None => err(format!("record has no field `{f}`")),
                    },
                    o => err(format!("cannot take `.{f}` of {}", o.kind())),
                }
            }
            ExprKind::App(f, args) => {
                let mut v = self.eval(m, inst, env, f)?;
                for a in args {
                    let x = self.eval(m, inst, env, a)?;
                    v = self.apply(v, x)?;
                }
                // Tag a query built in user code with where it was written:
                // for `q & stage` that is the stage, otherwise the whole call.
                // Prelude (module 0) stages are tagged at their user call site.
                // The tag is an explicit `CoreTerm::at`; nothing else in the
                // evaluation adds one, so erasure keeps today's `Rel::At`
                // wrappers exactly.
                if m != 0 {
                    if let Value::Query(t) = v {
                        let pipe = args.len() == 2
                            && matches!(&f.kind, ExprKind::Name(n) if crate::rules::is_pipe_name(n));
                        let span = if pipe { args[1].span } else { e.span };
                        v = Value::Query(Box::new(t.at(Loc { module: m, span })));
                    }
                }
                Ok(v)
            }
            ExprKind::Lambda(p, body) => Ok(Value::Closure(Rc::new(Closure {
                param: p.clone(),
                body: (**body).clone(),
                env: env.clone(),
                module: m,
                inst: inst.clone(),
            }))),
            ExprKind::Record(fs) => {
                let mut out: Vec<(String, Value)> = Vec::new();
                for (k, x) in fs {
                    if out.iter().any(|(o, _)| o == k) {
                        return err(format!("field `{k}` appears twice"));
                    }
                    let v = self.eval(m, inst, env, x)?;
                    out.push((k.clone(), v));
                }
                Ok(Value::Record(out))
            }
            ExprKind::List(xs) => Ok(Value::List(
                xs.iter()
                    .map(|x| self.eval(m, inst, env, x))
                    .collect::<EResult<_>>()?,
            )),
            ExprKind::Sql(_) => {
                err("`sql \"...\"` must be the whole body of a definition with a type signature")
            }
            ExprKind::Primitive(_) => err(
                "`primitive \"...\"` must be the whole body of a definition with a type signature",
            ),
            ExprKind::Error => err("syntax error"),
        }
    }

    pub fn apply(&mut self, f: Value, x: Value) -> EResult<Value> {
        match f {
            Value::Closure(c) => {
                if self.depth >= MAX_DEPTH {
                    let d = &self.ws.modules[c.module].module.defs[c.inst.def.1];
                    let msg = format!(
                        "`{}` evaluates more than {MAX_DEPTH} calls deep; \
                         recursion is not supported",
                        d.name
                    );
                    return at(err(msg), c.module, c.body.span);
                }
                self.depth += 1;
                let env = c.env.bind(c.param.clone(), x);
                let r = self.eval(c.module, &c.inst, &env, &c.body);
                self.depth -= 1;
                r
            }
            Value::Prim(p, mut args) => {
                args.push(x);
                if args.len() == p.arity() {
                    prims::call(p, args)
                } else {
                    Ok(Value::Prim(p, args))
                }
            }
            Value::Tpl(t, mut args) => {
                args.push(x);
                if args.len() == t.arity {
                    build_tpl(&t, args)
                } else {
                    Ok(Value::Tpl(t, args))
                }
            }
            o => err(format!("cannot apply {} to an argument", o.kind())),
        }
    }
}

/// `name : a -> b -> query r -> query r' = primitive "name"`: looks up the
/// primitive and returns it partially applied to the given arity.
fn primitive_def(name: &str, ty: Option<&TypeExpr>) -> EResult<Value> {
    let Some(_ty) = ty else {
        return err("a `primitive` needs a type signature, e.g. `where : query r -> (expr r bool) -> query r = primitive \"where\"`");
    };

    // Look up the primitive by name
    use crate::value::PRIMS;
    let prim = PRIMS
        .iter()
        .find(|(prim_name, _)| *prim_name == name)
        .map(|(_, p)| *p)
        .ok_or_else(|| EvalError {
            module: 0,
            span: None,
            message: format!("unknown primitive `{}`", name),
        })?;

    // Nullary primitives are values immediately; keeping them as a `Prim`
    // would make frame constants such as `currentRow` look like functions to
    // the runtime. Other primitives remain curried until their arguments are
    // supplied.
    if prim.arity() == 0 {
        prims::call(prim, vec![])
    } else {
        Ok(Value::Prim(prim, vec![]))
    }
}

/// `name : a -> b -> expr r t = sql "..."`: the signature gives the arity and
/// whether the result is a scalar, aggregate (`agg`), or window (`win`).
fn template_def(sql: &str, ty: Option<&TypeExpr>) -> EResult<Value> {
    let Some(ty) = ty else {
        return err("a `sql` template needs a type signature, e.g. `upper : expr r string -> expr r string = sql \"UPPER($1)\"`");
    };
    let (arity, result) = shape(ty);
    let kind = match result {
        TypeExpr::App { head, .. } if head == "agg" => TplKind::Agg,
        TypeExpr::App { head, .. } if head == "win" => TplKind::Win,
        _ => TplKind::Scalar,
    };
    let expr_args = if kind == TplKind::Win {
        match arity.checked_sub(1) {
            Some(n) => n,
            None => return err("a window template's first argument must be its window spec"),
        }
    } else {
        arity
    };
    check_placeholders(sql, expr_args)?;
    let t = Rc::new(Template {
        sql: sql.to_string(),
        kind,
        arity,
    });
    if arity == 0 {
        build_tpl(&t, vec![])
    } else {
        Ok(Value::Tpl(t, vec![]))
    }
}

fn shape(t: &TypeExpr) -> (usize, &TypeExpr) {
    match t {
        TypeExpr::Fun(_, r) => {
            let (n, res) = shape(r);
            (n + 1, res)
        }
        o => (0, o),
    }
}

/// A template's `$n` placeholders must be exactly `$1 ..= $expr_args`, each
/// standing alone. Anything else is a mistake that would otherwise surface as
/// missing SQL or a confusing error from the backend:
///
///   * `$0` is not a placeholder (they are 1-based), and used to pass silently
///     whenever a higher `$n` was present, because only the maximum was read.
///   * a gap (`$1` and `$3` but no `$2`) would leave one argument unused.
///   * a `$` with no digits — or a trailing `$` — is not a placeholder at all.
///   * `$n` inside a name or a string never reaches the backend as a
///     placeholder; `stage::template` rejects it, but the definition is the
///     better place to say so.
fn check_placeholders(sql: &str, expr_args: usize) -> EResult<()> {
    let b = sql.as_bytes();
    let mut seen = vec![false; expr_args + 1];
    let mut i = 0;
    let mut highest = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        // `$` inside a Cagara string literal cannot occur here (the lexer ends
        // the literal), so every `$` is a candidate placeholder.
        let start = i;
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == i + 1 {
            return err(format!(
                "SQL template `{sql}`: a `$` at byte {start} is not a placeholder \
                 (digits must follow it, as in `$1`)"
            ));
        }
        let n: usize = match sql[i + 1..j].parse() {
            Ok(n) => n,
            Err(_) => {
                return err(format!(
                    "SQL template `{sql}`: the placeholder `{}` is too large to be an \
                     argument position",
                    &sql[i..j]
                ))
            }
        };
        // `$n` may not be glued to a name or another `$`; `stage::template`
        // would leave it in the SQL instead of substituting it.
        let glued = |c: u8| c == b'_' || c.is_ascii_alphanumeric() || c == b'$';
        if b.get(j).copied().is_some_and(glued) || (i > 0 && glued(b[i - 1])) {
            return err(format!(
                "SQL template `{sql}`: `{}` must stand alone (not inside a name or string)",
                &sql[i..j]
            ));
        }
        if n == 0 {
            return err(format!(
                "SQL template `{sql}`: placeholders are 1-based, so `$0` is not an argument"
            ));
        }
        if n > expr_args {
            return err(format!(
                "template uses placeholders up to ${n} but its signature has {expr_args} \
                 expression argument(s)"
            ));
        }
        seen[n] = true;
        highest = highest.max(n);
        i = j;
    }
    if let Some(missing) = (1..=highest).find(|&n| !seen[n]) {
        return err(format!(
            "SQL template `{sql}`: `$1 .. ${highest}` are used but `${missing}` is missing; \
             placeholders must number every argument from 1"
        ));
    }
    if highest < expr_args {
        return err(format!(
            "template uses placeholders up to ${highest} but its signature has {expr_args} \
             expression argument(s)"
        ));
    }
    Ok(())
}

/// `t : query { a = int, b = string } = table "s" "t"` gives the table its
/// column list (in declaration order).
///
/// The columns are set only when they are not known yet, and `closed_row`
/// decides whether the signature supplies them at all — the same rule as
/// before, now stated on `CoreTerm`. The table may sit under `At` wrappers,
/// which `table_columns_mut` descends.
///
/// `Value::Query` holds a `Box<CoreTerm>`, so the mutation goes through the
/// box (`&mut **t`); the box is the unique owner here because the value was
/// just evaluated, which is also why nothing is cloned.
fn attach_schema(mut v: Value, ty: Option<&TypeExpr>) -> Value {
    if let Value::Query(t) = &mut v {
        if let Some(columns) = t.table_columns_mut() {
            if columns.is_none() {
                *columns = closed_row(ty);
            }
        }
    }
    v
}

fn closed_row(ty: Option<&TypeExpr>) -> Option<Vec<String>> {
    match ty? {
        TypeExpr::App { head, args, .. } if head == "query" && args.len() == 1 => match &args[0] {
            TypeExpr::Record {
                fields, tail: None, ..
            } => Some(fields.iter().map(|f| f.0.clone()).collect()),
            _ => None,
        },
        _ => None,
    }
}

/// Evaluate every definition of the root module and return the query ones,
/// schema-checked, in source order.
pub fn root_queries(ws: &Workspace) -> Vec<(String, Result<Rel, Diag>)> {
    root_queries_checked(ws, &crate::check::check(ws))
}

/// Like [`root_queries`], reusing a type check. A root definition with a type
/// error is reported (and not evaluated) even if it is not a query.
///
/// This is the one boundary where the compiler's IR becomes `Rel`: the erased
/// tree is what the SQL backend consumes and what `schema` validates.
///
/// # Two implementations, one result
///
/// There are two ways to reach the erased tree, and both run here:
///
/// * **source elaboration** (`crate::elaborate`), which walks each definition's
///   AST, builds `CheckedQuery` through its constructors, and erases it. This
///   is the path the design wants;
/// * **the evaluator**, which builds a `CoreTerm` and erases that
///   (`core_term::erase_core`). This is the long-standing path, kept as the
///   behavioural oracle.
///
/// They must agree, and the agreement is asserted here rather than assumed.
/// This function compares the two trees and **fails closed**: if they differ, or
/// if source elaboration cannot build the definition at all, that definition is
/// reported as an internal error and no `Rel` is returned for it.
///
/// Failing closed is a deliberate temporary policy, not the destination. The
/// destination is for source elaboration to be the only implementation, at which
/// point there is nothing to compare and this function is the elaborator. Until
/// then something has to happen when the two disagree, and the options are:
///
/// * **ship the evaluator's tree and warn** — keeps working programs working,
///   but a disagreement is exactly the case where the *compiler* is wrong and
///   neither tree can be assumed right; silently shipping one of them is how a
///   wrong answer reaches a user with a note they may never read;
/// * **fail closed** (chosen) — a disagreement is a compiler bug, and a compiler
///   bug that produces no output is a bug report, while one that produces
///   plausible SQL is a data incident.
///
/// The cost is real and is the reason this is called temporary: if elaboration
/// is wrong in a way that affects a definition, that definition stops compiling
/// even though the evaluator could have built it. The mitigation is coverage —
/// source elaboration handles every construct in the examples and in the test
/// corpus, and the parity check reports rather than guesses when it does not.
/// Preferring the evaluator *quietly* is the one option ruled out, because it
/// hides precisely the defect this comparison exists to find.
///
/// The comparison ignores `Rel::At` wrappers. Both erasers stamp them, from
/// different places (the evaluator from explicit `CoreTerm::At` nodes, the
/// checked layer from each node's own origin), and they are diagnostic sugar
/// over an otherwise identical tree. `schema_located` reads them, so they are
/// kept in the returned tree — only the equality check sets them aside.
///
/// `schema` stays here on purpose: it is still the guard rail over the erased
/// IR (and the second opinion a checked tree is compared against). It
/// re-derives column existence, which the `CoreTerm` constructors do not carry
/// rows for — `CoreTerm` is deliberately the row-less twin of `CheckedQuery`.
pub fn root_queries_checked(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>)> {
    let via_evaluator = root_queries_via_evaluator(ws, tc);
    let elaborated = crate::elaborate::elaborate_module(ws, tc, ws.root);

    let mut from_source: HashMap<&str, &Result<crate::checked::CheckedQuery, crate::core::Error>> =
        HashMap::new();
    for (name, r) in &elaborated {
        from_source.insert(name.as_str(), r);
    }

    let mut out = Vec::with_capacity(via_evaluator.len());
    for (name, result) in via_evaluator {
        let Ok(oracle) = &result else {
            // Already a diagnostic (type or evaluation error): nothing to
            // compare, and it is the user's error rather than an internal one.
            out.push((name, result));
            continue;
        };
        match from_source.get(name.as_str()) {
            // The elaborator produced nothing for this definition. That is the
            // normal case for a definition which is not a query at all, and
            // `None` is not evidence of a gap: the elaborator lists every
            // definition it walks, and this one is absent only when it had no
            // query to elaborate. The evaluator's relation is used.
            None => out.push((name, result)),
            // The elaborator *saw* a query here and could not build one.
            //
            // Because the evaluator produced a relation for this definition, the
            // failure is a capability gap in `elaborate.rs` and not a user error.
            // Fail closed: the definition is reported and no `Rel` is emitted
            // for it. See the policy note on this function for why this is the
            // temporary choice rather than shipping the oracle's tree.
            Some(Err(e)) => {
                let d = internal_disagreement(
                    ws,
                    &name,
                    &format!(
                        "source elaboration cannot handle this definition ({})",
                        e.message
                    ),
                );
                out.push((name, Err(d)));
            }
            Some(Ok(query)) => match crate::checked::erase((*query).clone()) {
                // Erasure is structural and total for query nodes, so this is
                // an internal inconsistency rather than a user error.
                Err(e) => {
                    let d = internal_disagreement(ws, &name, &e.message);
                    out.push((name, Err(d)));
                }
                Ok(rel) => {
                    if crate::checked::without_at(&rel) != crate::checked::without_at(oracle) {
                        let d = internal_disagreement(
                            ws,
                            &name,
                            "source elaboration and the evaluator produced different trees",
                        );
                        out.push((name, Err(d)));
                    } else {
                        // Agreed — the only case where a relation is emitted for
                        // a definition source elaboration handled. The
                        // evaluator's tree is the one kept: it carries the
                        // `Rel::At` wrappers `schema_located` blames spans with,
                        // and the checked tree is equal underneath.
                        out.push((name, result));
                    }
                }
            },
        }
    }
    out
}

/// A diagnostic for a disagreement between the two elaboration paths.
///
/// Worded as an internal error, not a program error: it means the compiler has
/// two implementations that must agree and do not. The definition name is
/// included because that is what a user can usefully report.
///
/// The wording says the program was *not* compiled, because that is what
/// fail-closed means and the message is the only place a user learns it. An
/// earlier version said the evaluator's result was used, which was true of the
/// intent and false of the code: every caller of this returns `Err`, so the
/// definition is dropped rather than shipped.
fn internal_disagreement(ws: &Workspace, name: &str, detail: &str) -> Diag {
    let m = ws.root;
    let span = ws.modules[m]
        .module
        .defs
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.span)
        .unwrap_or(cagara_syntax::ast::Span { start: 0, end: 0 });
    ws.diag_span(
        m,
        span,
        format!(
            "internal error in `{name}`: {detail}. This definition was not compiled, because the \
             compiler's two elaboration paths disagree and neither can be trusted here; please \
             report this"
        ),
    )
}

/// The evaluator's path to the erased tree: evaluate, erase the `CoreTerm`,
/// then validate columns. See [`root_queries_checked`], which wraps this with
/// the source-elaboration comparison.
///
/// # Why this is reachable at all
///
/// A differential test that builds its oracle by calling
/// [`root_queries_checked`] is **circular**: that function already runs source
/// elaboration and, on disagreement, replaces the result with an error. A
/// harness that then filters errors out would silently drop exactly the
/// definitions it exists to compare — so a mismatch would look like a
/// definition the evaluator could not handle, and pass.
///
/// The harness therefore compares against *this*, which is the evaluator with no
/// source elaboration involved at all.
///
/// It is `pub(crate)`, not `pub`: its only callers are the comparison inside
/// [`root_queries_checked`] and the tests of that comparison, and it is not part
/// of the compiler's contract. Making it crate-private means another crate
/// cannot reach past the parity check to the raw evaluator — which is the
/// property worth having, since reaching past it is exactly how a caller would
/// bypass the fail-closed behaviour without noticing.
pub(crate) fn root_queries_via_evaluator(
    ws: &Workspace,
    tc: &TypeCheck,
) -> Vec<(String, Result<Rel, Diag>)> {
    let mut ev = Evaluator::new(ws, tc);
    let m = ws.root;
    let mut out = Vec::new();
    for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
        if let Some(e) = tc.error_for(m, i) {
            out.push((d.name.clone(), Err(e.clone())));
            continue;
        }
        if tc.holes(m, i) > 0 {
            // Overloaded helper: only meaningful at its uses.
            continue;
        }
        match ev.def_value(m, i) {
            Ok(Value::Query(t)) => {
                // Erasure is fallible: a `CoreTerm` is an open sum, and a
                // definition that elaborated to something which is not a query
                // is an internal error rather than a query with no columns.
                let checked = match crate::core_term::erase_core(*t) {
                    Err(e) => Err(ws.diag_span(m, d.span, e.message)),
                    Ok(rel) => match crate::schema::schema_located(&rel) {
                        Ok(_) => Ok(rel),
                        Err((Some(l), msg)) => Err(ws.diag_span(l.module, l.span, msg)),
                        Err((None, msg)) => Err(ws.diag_span(m, d.span, msg)),
                    },
                };
                out.push((d.name.clone(), checked));
            }
            Ok(_) => {}
            Err(e) => out.push((d.name.clone(), Err(ws.eval_diag(&e)))),
        }
    }
    out
}

/// Evaluate every definition of the root module and return the query ones as
/// the *core* term, before erasure. The entry point for callers that want to
/// inspect the elaboration rather than only its `Rel` encoding.
pub fn root_core_terms(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<CoreTerm, Diag>)> {
    let mut ev = Evaluator::new(ws, tc);
    let m = ws.root;
    let mut out = Vec::new();
    for (i, d) in ws.modules[m].module.defs.iter().enumerate() {
        if let Some(e) = tc.error_for(m, i) {
            out.push((d.name.clone(), Err(e.clone())));
            continue;
        }
        if tc.holes(m, i) > 0 {
            continue;
        }
        match ev.def_value(m, i) {
            Ok(Value::Query(t)) => out.push((d.name.clone(), Ok(*t))),
            Ok(_) => {}
            Err(e) => out.push((d.name.clone(), Err(ws.eval_diag(&e)))),
        }
    }
    out
}

/// What elaboration produced for one definition, and why.
///
/// This is the term contract, made explicit rather than inferred from the
/// absence of an entry in a map:
///
/// * [`Elaborated::Query`] — a query definition, with one core term per
///   assignment of its overload holes (an empty hole list means one term).
/// * [`Elaborated::Value`] — a definition that evaluated to something that is
///   not a query: a scalar, a function, a record. It has no *relational* term,
///   which is a fact about the definition, not a failure.
/// * [`Elaborated::OpenHoles`] — the definition leaves open overloads, so it is
///   meaningful only at its uses and has no body of its own. Its uses carry the
///   choices, recorded by `TypeCheck::choices_of`.
/// * [`Elaborated::Failed`] — it could not be evaluated (or did not type-check).
///   The reason is returned alongside, so a caller is never left guessing.
///
/// Previously a definition in any of the last three cases simply had no entry,
/// so "a scalar", "an open-hole helper", and "evaluation blew up" were the same
/// observation: a missing key. A caller could not report the third at all.
#[derive(Debug, Clone, PartialEq)]
pub enum Elaborated {
    /// A query, with one term per overload-hole assignment.
    Query(Vec<CoreTerm>),
    /// Evaluated, but not to a query: no relational term exists.
    Value,
    /// Leaves open overloads; its uses carry the choices.
    OpenHoles,
    /// Did not type-check or did not evaluate.
    Failed,
}

/// Elaborate every definition of every module, for
/// [`crate::CheckedProgram::of_elaborated`].
///
/// Returns one entry per definition that was *considered*, including the ones
/// that produced no term, plus the diagnostics raised while evaluating. Both
/// halves matter: the previous version returned only successful query terms, so
/// an evaluator error was silently dropped and "not a query" was
/// indistinguishable from "failed".
///
/// Definitions that did not type-check are reported as [`Elaborated::Failed`]
/// with no diagnostic here, because the checker's diagnostic is already in
/// `CheckedProgram::diagnostics` and duplicating it would report one error
/// twice.
pub fn elaborate_bodies(
    ws: &Workspace,
    tc: &TypeCheck,
) -> (HashMap<(usize, usize), Elaborated>, Vec<crate::core::Diagnostic>) {
    let mut ev = Evaluator::new(ws, tc);
    let mut out = HashMap::new();
    let mut diags = Vec::new();
    for m in 0..ws.modules.len() {
        for (i, _d) in ws.modules[m].module.defs.iter().enumerate() {
            // A definition the checker already rejected is `Failed`; its
            // diagnostic comes from the checker, not from here.
            if tc.error_for(m, i).is_some() {
                out.insert((m, i), Elaborated::Failed);
                continue;
            }
            // A definition with open holes is meaningful only at its uses.
            if tc.holes(m, i) > 0 {
                out.insert((m, i), Elaborated::OpenHoles);
                continue;
            }
            match ev.def_value(m, i) {
                Ok(Value::Query(t)) => {
                    out.insert((m, i), Elaborated::Query(vec![*t]));
                }
                Ok(_) => {
                    out.insert((m, i), Elaborated::Value);
                }
                Err(e) => {
                    // The evaluator's own diagnostic, reported rather than
                    // dropped: this is the half the old version lost.
                    //
                    // `e.module` is where the failure was *raised*, which is
                    // not always the module being evaluated: a definition here
                    // can fail inside an imported one (a prelude helper, an
                    // aliased module). Recording the outer loop's `i` against
                    // `e.module` would then pair a module with a definition
                    // index that does not exist in it. Attribute the
                    // definition by the error's span within its own module,
                    // and fall back to no definition rather than a wrong one.
                    let def = e.span.and_then(|span| {
                        ws.modules
                            .get(e.module)
                            .and_then(|md| {
                                md.module
                                    .defs
                                    .iter()
                                    .position(|d| {
                                        d.span.start <= span.start && span.end <= d.span.end
                                    })
                            })
                    });
                    diags.push(crate::core::Diagnostic::new(
                        e.module,
                        def,
                        ws.eval_diag(&e),
                    ));
                    out.insert((m, i), Elaborated::Failed);
                }
            }
        }
    }
    (out, diags)
}

/// The elaboration tests, a sibling of `tests` rather than a child of it: they
/// assert the *shape* of the emitted core term and pin each primitive's
/// argument order (a permuted `Vec<Value>` is invisible to the type system).
#[cfg(test)]
mod elaboration_tests;

#[cfg(test)]
mod tests {
    use crate::root_queries_checked;
    use crate::workspace::Workspace;

    /// Diagnostics the whole pipeline produces for a root-only source file.
    fn errors(src: &str) -> Vec<String> {
        let ws = Workspace::from_source(src);
        let tc = crate::check::check(&ws);
        let mut out: Vec<String> = ws.diags.iter().map(|d| d.message.clone()).collect();
        out.extend(tc.errors.iter().map(|e| e.diag.message.clone()));
        out.extend(
            root_queries_checked(&ws, &tc)
                .into_iter()
                .filter_map(|(_, r)| r.err())
                .map(|d| d.message),
        );
        out
    }

    #[test]
    fn self_application_is_reported_instead_of_overflowing() {
        // `f` returns a closure, so the definition cycle check does not fire:
        // each application re-enters `f` for ever. This used to overflow the
        // stack (and take the language server down with it).
        let e = errors("f = x => f x\nq = f 1\n");
        assert!(
            e.iter().any(|m| m.contains("recursion is not supported")),
            "{e:?}"
        );
    }

    #[test]
    fn a_long_chain_of_definitions_is_reported_instead_of_overflowing() {
        // Acyclic, so the cycle check does not fire, and forward-referencing,
        // so evaluation enters the chain from the top before anything is
        // cached: each alias costs a nesting level. (In source order the
        // chain is evaluated bottom-up and never nests.) The checker reports
        // its own forward-reference limit too; this pins the evaluator's.
        let mut src = String::from("q = a99\n");
        for i in 0..100 {
            if i == 0 {
                src.push_str("a0 = 1\n");
            } else {
                src.push_str(&format!("a{i} = a{}\n", i - 1));
            }
        }
        let e = errors(&src);
        assert!(
            e.iter()
                .any(|m| m.contains("used through a chain of more than")),
            "{e:?}"
        );
    }

    #[test]
    fn mutual_application_is_reported_too() {
        let e = errors("f = x => g x\ng = x => f x\nq = f 1\n");
        assert!(!e.is_empty(), "expected a recursion error");
    }

    #[test]
    fn deep_but_finite_helpers_still_evaluate() {
        // A chain of ordinary helpers is not recursion.
        let mut src = String::from("id = x => x\n");
        for i in 0..64 {
            src.push_str(&format!("h{i} = x => id x\n"));
        }
        src.push_str("q = h0 1\n");
        assert!(errors(&src).is_empty(), "{:?}", errors(&src));
    }

    #[test]
    fn too_many_open_overloads_is_a_diagnostic_not_a_panic() {
        // A definition leaving more open overloads than an encoded origin can
        // hold used to `panic!`. 257 is one past the limit.
        let fields: Vec<String> = (0..257).map(|i| format!("f{i} = x + x")).collect();
        let src = format!(
            "h = x => {{ {} }}\nt : query {{ a = int }} = table \"p\" \"t\"\nr = h 1\nq = t & select {{ v = .a }}\n",
            fields.join(", ")
        );
        let e = errors(&src);
        assert!(
            e.iter().any(|m| m.contains("too many open overloads")),
            "{e:?}"
        );
    }

    /// A two-argument template with `sql` substituted into it, used by a query
    /// so the definition is evaluated rather than only type-checked.
    fn template_errors(sql: &str) -> Vec<String> {
        errors(&format!(
            "t : query {{ a = int, b = int }} = table \"s\" \"t\"\n\
             f : expr r int -> expr r int -> expr r int = sql \"{sql}\"\n\
             q = t & select {{ y = f .a .b }}\n"
        ))
    }

    #[test]
    fn template_placeholder_zero_is_rejected() {
        // `$0` is not an argument, but used to pass whenever a higher `$n` was
        // present, because only the maximum placeholder was read: `$1 + $0`
        // silently compiled and left the `$0` out of the SQL.
        let e = template_errors("$1 + $0");
        assert!(e.iter().any(|m| m.contains("1-based")), "{e:?}");
        assert!(
            template_errors("$1 + $2 + $0")
                .iter()
                .any(|m| m.contains("1-based")),
            "$0 must be rejected even beside a valid higher placeholder"
        );
    }

    #[test]
    fn template_placeholder_gaps_are_rejected() {
        // `$1` and `$3` with no `$2` leaves one argument unused. Previously the
        // maximum matched the arity, so this reached the backend, which
        // reported only that the template could not be substituted.
        let src = "t : query { a = int, b = int, c = int } = table \"s\" \"t\"\n\
                   f : expr r int -> expr r int -> expr r int -> expr r int = sql \"$1 + $3\"\n\
                   q = t & select { y = f .a .b .c }\n";
        let e = errors(src);
        assert!(e.iter().any(|m| m.contains("`$2` is missing")), "{e:?}");
    }

    #[test]
    fn a_bare_dollar_is_rejected() {
        // A trailing `$` used to be ignored outright, so the template compiled
        // with the `$` left in the SQL.
        for sql in ["$1 + $", "$1 + $$2", "$ 1", "$"] {
            let e = template_errors(sql);
            assert!(
                e.iter().any(|m| m.contains("not a placeholder")),
                "{sql}: {e:?}"
            );
        }
    }

    #[test]
    fn a_placeholder_inside_a_name_is_rejected_at_the_definition() {
        // The backend rejects these too, but only once the definition is used
        // and only with a generic message; the definition is the better place.
        for sql in ["x$1 + $2", "$1x + $2", "$1_$2"] {
            let e = template_errors(sql);
            assert!(
                e.iter().any(|m| m.contains("must stand alone")),
                "{sql}: {e:?}"
            );
        }
    }

    #[test]
    fn well_formed_templates_are_accepted() {
        // Every argument, in any order, with placeholders standing alone.
        assert!(template_errors("$1 + $2").is_empty());
        assert!(template_errors("$2 - $1").is_empty());
        assert!(template_errors("CAST($1 AS int) + $2").is_empty());
    }
}
