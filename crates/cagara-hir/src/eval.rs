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
/// This is the one boundary where the evaluator's `CoreTerm` becomes `Rel`:
/// the erased tree is what the SQL backend consumes and what `schema`
/// validates. Erasure is total and structural (`core_term::erase_core`), so a
/// query that evaluated successfully erases to exactly the `Rel` the evaluator
/// used to build by hand — including the `Rel::At` wrappers that
/// `ExprKind::App` adds for `q & stage`, which is what lets `schema_located`
/// blame the innermost failing stage.
///
/// `schema` stays here on purpose: it is still the guard rail over the erased
/// IR (and the second opinion a checked tree is compared against in tests). It
/// re-derives column existence, which the `CoreTerm` constructors do not carry
/// rows for — `CoreTerm` is deliberately the row-less twin of `CheckedQuery`.
pub fn root_queries_checked(ws: &Workspace, tc: &TypeCheck) -> Vec<(String, Result<Rel, Diag>)> {
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

/// Elaborate every definition of every module to its core term(s), for
/// [`crate::CheckedProgram::of_elaborated`].
///
/// A definition with no open overloads has exactly one body. A definition that
/// leaves `n` open overloads is meaningful only at its uses, so it gets no
/// body of its own here — the *uses* carry the choices, and
/// `CheckedProgram` records those separately via `TypeCheck::choices_of`.
/// A definition that fails to evaluate is skipped rather than given a
/// placeholder: its diagnostic is already in `CheckedProgram::diagnostics`.
///
/// Errors are deliberately not returned. A definition that does not evaluate is
/// a diagnostic the caller already has, and fabricating a term for it is
/// exactly the failure mode this refactor is removing.
pub fn elaborate_bodies(
    ws: &Workspace,
    tc: &TypeCheck,
) -> HashMap<(usize, usize), Vec<CoreTerm>> {
    let mut ev = Evaluator::new(ws, tc);
    let mut out = HashMap::new();
    for m in 0..ws.modules.len() {
        for (i, _d) in ws.modules[m].module.defs.iter().enumerate() {
            if tc.error_for(m, i).is_some() || tc.holes(m, i) > 0 {
                continue;
            }
            if let Ok(Value::Query(t)) = ev.def_value(m, i) {
                out.insert((m, i), vec![*t]);
            }
        }
    }
    out
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
