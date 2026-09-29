//! Compile-time evaluator. Runs a Cagara program (prelude included) and
//! reduces each definition to a value; query definitions become `Rel` IR.

use crate::ir::{Expr, Lit, Rel};
use crate::prims::{self, build_tpl};
use crate::check::{Choice, TypeCheck};
use crate::value::{err, Closure, EResult, Env, Inst, Template, TplKind, Value};
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
}

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
        Evaluator { ws, tc, cache: HashMap::new(), active: Vec::new() }
    }

    /// Value of a definition with no open overloads.
    pub fn def_value(&mut self, m: usize, i: usize) -> EResult<Value> {
        self.inst_value(Inst { def: (m, i), holes: vec![] })
    }

    fn inst_value(&mut self, inst: Inst) -> EResult<Value> {
        if let Some(v) = self.cache.get(&inst) {
            return Ok(v.clone());
        }
        let ws = self.ws;
        let (m, i) = inst.def;
        let def = &ws.modules[m].module.defs[i];
        if self.active.contains(&(m, i)) {
            let msg = format!("`{}` refers to itself; recursion is not supported", def.name);
            return at(err(msg), m, def.span);
        }
        self.active.push((m, i));
        let r = self.eval_def(m, def, Rc::new(inst.clone()));
        self.active.pop();
        let v = r?;
        self.cache.insert(inst, v.clone());
        Ok(v)
    }

    fn eval_def(&mut self, m: usize, def: &ast::Def, inst: Rc<Inst>) -> EResult<Value> {
        let v = match &def.body.kind {
            ExprKind::Sql(sql) => at(template_def(sql, def.ty.as_ref()), m, def.body.span)?,
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
        let holes = (0..self.tc.holes(dm, di)).map(|k| self.choose(inst, site, k)).collect::<EResult<_>>()?;
        self.inst_value(Inst { def: (dm, di), holes })
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
            Binding::Module(_) => err(format!("`{n}` is a module; refer to a definition as `{n}.name`")),
        }
    }

    fn eval_inner(&mut self, m: usize, inst: &Rc<Inst>, env: &Env, e: &ast::Expr) -> EResult<Value> {
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
            ExprKind::Field(side, n) => Ok(Value::Expr(Expr::Col(*side, n.clone()))),
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
            ExprKind::List(xs) => {
                Ok(Value::List(xs.iter().map(|x| self.eval(m, inst, env, x)).collect::<EResult<_>>()?))
            }
            ExprKind::Sql(_) => {
                err("`sql \"...\"` must be the whole body of a definition with a type signature")
            }
            ExprKind::Error => err("syntax error"),
        }
    }

    pub fn apply(&mut self, f: Value, x: Value) -> EResult<Value> {
        match f {
            Value::Closure(c) => {
                let env = c.env.bind(c.param.clone(), x);
                self.eval(c.module, &c.inst, &env, &c.body)
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
    let used = max_placeholder(sql);
    if used != expr_args {
        return err(format!(
            "template uses placeholders up to ${used} but its signature has {expr_args} expression argument(s)"
        ));
    }
    let t = Rc::new(Template { sql: sql.to_string(), kind, arity });
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

fn max_placeholder(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut max = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let j = (i + 1..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
            if let Ok(n) = sql[i + 1..j].parse::<usize>() {
                max = max.max(n);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    max
}

/// `t : query { a = int, b = string } = table "s" "t"` gives the table its
/// column list (in declaration order).
fn attach_schema(mut v: Value, ty: Option<&TypeExpr>) -> Value {
    if let Value::Query(Rel::Table { columns, .. }) = &mut v {
        if columns.is_none() {
            *columns = closed_row(ty);
        }
    }
    v
}

fn closed_row(ty: Option<&TypeExpr>) -> Option<Vec<String>> {
    match ty? {
        TypeExpr::App { head, args, .. } if head == "query" && args.len() == 1 => match &args[0] {
            TypeExpr::Record { fields, tail: None, .. } => Some(fields.iter().map(|f| f.0.clone()).collect()),
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
            Ok(Value::Query(r)) => {
                let checked = match crate::schema::schema(&r) {
                    Ok(_) => Ok(r),
                    Err(msg) => Err(ws.diag(m, d.span.start as usize, msg)),
                };
                out.push((d.name.clone(), checked));
            }
            Ok(_) => {}
            Err(e) => out.push((d.name.clone(), Err(ws.eval_diag(&e)))),
        }
    }
    out
}
