//! [`Checker`] inference, annotations, definitions, and constraint solving.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

use crate::rules::{self, Place};

/// Is this a key parameter — the position a stage reads its key from?
///
/// `omit` / `prefix` / `suffix` take a `symbol k`, which `conv` carries as a
/// `keyLabel` around a type variable; the call site binds that variable to the
/// literal's `Sym`. The bare marker constructors are the older spelling of the
/// same position and are recognised so a hand-written signature using one still
/// gets the stage's own error rather than a bare type mismatch.
fn is_key_parameter(p: &Ty) -> bool {
    matches!(p, Ty::Con("keyLabel" | "valueLabel", _))
}

/// The label a key parameter declares: the type variable inside `keyLabel`, or
/// the marker itself for a hand-written signature that uses one.
fn key_label(p: &Ty) -> Ty {
    match p {
        Ty::Con("keyLabel" | "valueLabel", args) if args.len() == 1 => args[0].clone(),
        other => other.clone(),
    }
}

/// Can this argument name a column or a label fragment?
///
/// Two forms can: a literal (the use-site spelling, `prefix "u_"`) and a bare
/// name (a helper's own parameter, `addPrefix = p => q => q & prefix p`). A
/// computed string cannot name a column, so it is refused rather than deferred.
fn is_key_argument(arg: &ast::Expr) -> bool {
    matches!(
        arg.kind,
        ExprKind::Lit(ast::Lit::Str(_)) | ExprKind::Name(_)
    )
}

/// Why a key argument has to be a label, and what to write instead.
///
/// The message names the stage when the source does, because that is what the
/// reader is looking at; a helper whose parameter is a label gets the generic
/// wording, since the helper is not the stage that applies the mapping.
fn key_argument_message(f: &ast::Expr) -> String {
    match &f.kind {
        ExprKind::Name(n) if n == "omit" => {
            "`omit` needs a literal column name, such as `omit \"password_hash\"`: the \
             column must be known when the query is checked"
                .into()
        }
        ExprKind::Name(n) if n == "prefix" => {
            "`prefix` needs a literal string, such as `prefix \"user_\"`: the mapping is \
             part of the type, so it must be known when the query is checked"
                .into()
        }
        ExprKind::Name(n) if n == "suffix" => {
            "`suffix` needs a literal string, such as `suffix \"_v2\"`: the mapping is part \
             of the type, so it must be known when the query is checked"
                .into()
        }
        ExprKind::Name(n) if n == "mapValue" => {
            "`mapValue` needs a literal wrapper, such as `mapValue \"nullable\"` or \
             `mapValue \"list\"`"
                .into()
        }
        _ => "a key argument must be a literal string, such as `prefix \"user_\"`: the \
              mapping is part of the type, so it must be known when the query is checked"
            .into(),
    }
}

impl<'w> Checker<'w> {
    // ── definitions and schemes ────────────────────────────────────────────

    pub(crate) fn def_scheme(&mut self, m: usize, i: usize) -> Option<Scheme> {
        if m != self.module {
            return self.env.schemes.get(&m)?.get(&(m, i)).cloned();
        }
        if let Some(s) = self.schemes.get(&(m, i)) {
            return Some(s.clone());
        }
        if self.active.contains(&(m, i)) {
            // Recursive definition.
            return None;
        }
        if self.depth >= MAX_DEF_DEPTH {
            // The chain of forward references is deeper than any real program
            // and would overflow the stack before a diagnostic could be
            // reported, so report it once, here, and let the rest of the
            // chain check against a fresh type instead of cascading.
            if !self.depth_reported {
                self.depth_reported = true;
                self.errors.push(RawTypeError {
                    module: m,
                    def: i,
                    span: self.env.defs[i].span,
                    message: format!(
                        "`{}` lies on a chain of more than {MAX_DEF_DEPTH} forward-referenced \
                         definitions; define a name before the definitions that use it",
                        self.env.defs[i].name
                    ),
                });
            }
            let v = self.fresh();
            let scheme = self.generalize(&v, vec![]);
            self.schemes.insert((m, i), scheme.clone());
            return Some(scheme);
        }
        self.active.push((m, i));
        self.depth += 1;
        let saved = std::mem::take(&mut self.pending);
        let saved_uses = std::mem::take(&mut self.uses);
        let saved_stages = std::mem::take(&mut self.row_stages);
        let r = self.check_def(i);
        self.depth -= 1;
        // Use types as far as checking got, even if it failed later on. The
        // three maps below are the typed tree's inputs: the facts keyed by
        // `ExprId`, the printed form, and where each use was filed.
        let mut typed_facts: HashMap<u32, Ty> = HashMap::new();
        let mut shown_by_id: HashMap<u32, String> = HashMap::new();
        // The filing span, plus the position in the recording order the tree's
        // last-write tie-break needs. A node recorded twice keeps its last
        // entry, which is also the `shown` its `shown_by_id` entry holds.
        let mut recorded_by_id: HashMap<u32, (Span, usize)> = HashMap::new();
        for (write, (id, sp, t, lit)) in std::mem::replace(&mut self.uses, saved_uses)
            .into_iter()
            .enumerate()
        {
            let shown = match (lit, self.resolve(&t)) {
                (Some(l), Ty::Var(_)) => l.to_string(),
                _ => self.show(&t),
            };
            shown_by_id.insert(id, shown);
            recorded_by_id.insert(id, (sp, write));
            // The type as the node got it, which is what the tree stores. A
            // phase that walks the AST asks "what type did *this* node get?";
            // the printed form above cannot answer that — it is a string.
            //
            // **Deeply** resolved, not just at the top. `resolve` chases a
            // variable chain and stops, so `query r` came back with `r` still a
            // variable while the printed form showed `query { id = int }` — the
            // row was recorded and then unreachable, which is how a `table`'s
            // columns became something a consumer had to recover from the
            // definition's signature instead of reading off the node. `zonk`
            // resolves inside constructors, rows, and function arrows, so what
            // is stored here is the type the node actually got.
            typed_facts.insert(id, self.zonk(&t));
        }
        // Whatever the definition learned about the probe's row, even if it
        // failed later on. Only the definition that actually referenced the
        // probe may record it, and it must *not* consume the probe otherwise:
        // with more than one definition checked in a module, a plain `take()`
        // let a later definition steal the probe and either replace a known
        // row with its own empty one or discard the answer outright.
        if self.probe.as_ref().is_some_and(|(owner, _)| *owner == i) {
            let (_, p) = self.probe.take().expect("checked just above");
            let (fs, _) = self.flatten(&p);
            self.probe_fields = Some(fs.iter().map(|(k, t)| (k.clone(), self.show(t))).collect());
        }
        let left = std::mem::replace(&mut self.pending, saved);
        self.active.pop();
        // The stage nodes this definition recorded: what each stage application
        // read and produced. They are closed here, once the definition's rows
        // are solved, and dropped for a definition that failed.
        let stages = self.finish_row_stages();
        let succeeded = r.is_ok();
        let scheme = match r {
            Ok(t) => {
                // Constraints can bind the row after inference has built a
                // symbolic `keyMap` term. Normalize the completed type before
                // generalization so closed rows expose their mapped fields
                // instead of retaining a redundant wrapper around them.
                let t = self.zonk(&t);
                // Open overloads become holes, numbered in constraint order.
                let mut cons = Vec::new();
                let mut k = 0;
                for (c, _) in &left {
                    let mut c = c.map(&mut |t| self.zonk(t));
                    if let Cons::Overload { origin, .. } = &mut c {
                        if let Origin::Site(site) = *origin {
                            let (use_site, hk) = decode(site);
                            self.record((m, i), use_site, hk, Choice::Hole(k));
                        }
                        *origin = Origin::Hole(k);
                        k += 1;
                    }
                    cons.push(c);
                }
                if k > 0 {
                    self.holes.insert((m, i), k);
                }
                let mut scheme = self.generalize(&t, cons);
                scheme.stage = self.def_stage(i);
                scheme
            }
            Err(e) => {
                self.errors.push(RawTypeError {
                    module: m,
                    def: i,
                    span: e.span,
                    message: e.msg,
                });
                self.failed.insert((m, i));
                let v = self.fresh();
                Scheme {
                    failed: true,
                    ..self.generalize(&v, vec![])
                }
            }
        };
        // The tree is built *after* generalization: the open overloads
        // become holes there, and a tree built earlier would report a
        // definition with none.
        // The same facts as a tree over the body, which is what a consumer
        // walks: the maps above are read by `ExprId`, which is a question only
        // a consumer that already knows the id can ask.
        //
        // A definition that failed contributes no stage nodes, exactly as it
        // contributes none to the tree's stages: the chain is closed only when
        // the definition's rows are solved.
        if !typed_facts.is_empty() {
            let body = &self.env.defs[i].body;
            let holes = self.holes.get(&(m, i)).copied().unwrap_or(0);
            let recorded = self.choices.get(&(m, i)).cloned().unwrap_or_default();
            let tree = crate::check::typed::build(
                body,
                holes,
                &crate::check::typed::NodeFacts {
                    stages: if succeeded { stages.as_slice() } else { &[] },
                    view: &|id| typed_facts.get(&id).map(scheme_view),
                    shown: &|id| shown_by_id.get(&id).cloned(),
                    recorded: &|id| recorded_by_id.get(&id).copied(),
                    resolved: &|id| self.resolutions.get(&(m, id)).cloned(),
                    choices: &|id| {
                        // The choices recorded *at* this node's site, by hole.
                        let mut at: Vec<(usize, Choice)> = recorded
                            .iter()
                            .filter(|((site, _), _)| *site == id)
                            .map(|((_, hole), choice)| (*hole, *choice))
                            .collect();
                        at.sort_unstable_by_key(|(hole, _)| *hole);
                        at
                    },
                },
            );
            self.typed_bodies.insert((m, i), tree);
        }

        self.row_stages = saved_stages;
        self.schemes.insert((m, i), scheme.clone());
        self.fit_cache.clear();
        Some(scheme)
    }

    /// A clearer message for the one signature mistake the unifier can only
    /// describe from its own point of view.
    ///
    /// When a body's row has a column its signature does not, the two rows
    /// unify one side at a time and the failure surfaces as `no column `x`;
    /// available: …` over the *signature's* columns — naming a column that does
    /// exist and listing the very set that is short. A reader takes that as a
    /// complaint about the query, which is the opposite of what is wrong. Only
    /// the signature comparison knows which side is the declaration, so the
    /// case is detected here instead.
    ///
    /// A signature that names a column the body has not is left to the
    /// unifier: there the ordinary wording is already right.
    ///
    /// A `keyMap` annotation is left to the unifier too. Its row is the *input*
    /// to the rename, so comparing it with the body's row here would report
    /// every renamed column as missing; `coerce` reduces the term first and
    /// then compares the rows that actually correspond.
    pub(crate) fn signature_missing_columns(&self, inferred: &Ty, ann: &Ty) -> Option<String> {
        let (Ty::Con("query", got), Ty::Con("query", want)) =
            (self.resolve(inferred), self.resolve(ann))
        else {
            return None;
        };
        let (got, want) = (got.first()?.clone(), want.first()?.clone());
        // Two shapes are left to the unifier.
        //
        // A `keyMap` signature names the *input* row of a rename, so comparing
        // it with the body's row here would call every renamed column missing;
        // `coerce` reduces the term and compares the rows that correspond.
        //
        // A body whose row still carries a `keyMap` term is itself still
        // constrainable — the signature may legitimately narrow it, and
        // `unify_rows` lets it: `q : query { u_id_v2 = int } = users & prefix
        // "u_" & suffix "_v2"` is well typed even though the pipeline alone
        // produces four columns. Only a body whose row is already fixed can a
        // signature be too narrow for.
        if matches!(self.resolve(&want), Ty::MapKey(..)) || self.keymap_on_spine(&got) {
            return None;
        }
        let (ifields, itail) = self.flatten(&got);
        let (afields, atail) = self.flatten(&want);
        // Only when both rows are fully known: an open one may still grow the
        // missing columns on its own.
        if itail != Ty::Empty || atail != Ty::Empty {
            return None;
        }
        let missing: Vec<&str> = ifields
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| !afields.iter().any(|(k, _)| k == n))
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "the signature is missing column `{}`; the query has: {}",
            missing.join("`, `"),
            names(&ifields)
        ))
    }

    pub(crate) fn check_def(&mut self, i: usize) -> R<Ty> {
        let defs = self.env.defs;
        let def = &defs[i];
        let ann = match &def.ty {
            Some(t) => Some(self.annotation(t).map_err(at(def.span))?),
            None => None,
        };
        if let ExprKind::Sql(_) = def.body.kind {
            // A template's signature is its type: its arity and whether it is
            // a scalar, aggregate, or window function come from the arrows, so
            // there is nothing to infer.
            return match ann {
                Some(t) => Ok(t),
                None => Err(at(def.span)(format!(
                    "`{}` needs a type signature: a `sql` template takes its arity and phase \
                     from it, e.g. `{} : expr r string -> expr r string = sql \"UPPER($1)\"`",
                    def.name, def.name
                ))),
            };
        }
        if let ExprKind::Primitive(_) = def.body.kind {
            // A primitive's signature is its type: the type annotation is required
            // and describes the primitive's behavior.
            return match ann {
                Some(t) => {
                    // A **projection** primitive exposes `expr r (row s)` (or,
                    // for the aggregate stage, `agg (expr r (row s))`) at the
                    // surface, while its internal type carries the row of field
                    // expressions separately so the constraint solver can
                    // validate and lower the stage. Every other primitive's
                    // signature *is* its type, and forcing the internal form on
                    // one of those is a type error — which is why this asks the
                    // table rather than mapping every name through it.
                    let internal = match &def.body.kind {
                        ExprKind::Primitive(name) => crate::primitive::prim_of(name)
                            .filter(|p| p.is_projection() || p.owns_row_rule())
                            .map(|p| self.prim_type(p, def.body.span)),
                        _ => None,
                    };
                    if let Some(internal) = internal {
                        if matches!(
                            &def.body.kind,
                            ExprKind::Primitive(name)
                                if matches!(
                                    name.as_str(),
                                    "__innerJoin" | "__leftJoin" | "__rightJoin"
                                        | "__fullJoin" | "__semiJoin" | "__antiJoin"
                                )
                        ) {
                            return Ok(internal);
                        }
                        self.span = def.body.span;
                        self.coerce(&internal, &t)
                            .map_err(|msg| {
                                format!("`{}` does not match its signature: {msg}", def.name)
                            })
                            .map_err(at(def.body.span))?;
                        return Ok(t);
                    }
                    Ok(t)
                }
                None => Err(at(def.span)(format!(
                    "`{}` needs a type signature: a `primitive` takes its type \
                     from it, e.g. `{} : query r -> query r' = primitive \"__prefix\"`",
                    def.name, def.name
                ))),
            };
        }
        let t = self.infer(&mut Vec::new(), &def.body)?;
        if let Some(a) = &ann {
            self.span = def.body.span;
            if let Some(msg) = self.signature_missing_columns(&t, a) {
                return Err(at(def.body.span)(msg));
            }
            self.coerce(&t, a)
                .map_err(|msg| format!("`{}` does not match its signature: {msg}", def.name))
                .map_err(at(def.body.span))?;
            // A literal body keeps the default kind `infer` recorded for it.
            // `coerce` accepts `"x"` against `date` because a string literal
            // widens, but widening is not a *type* the literal was recorded
            // with, so
            //
            //     userInputDate : date = "2025-12-31"
            //
            // read the literal back as `string` -- the definition said `date`
            // and its own body disagreed. `check` re-records a literal at the
            // type its expectation demands (see the note there); a definition
            // body reaches its type by `coerce` instead, so it re-records the
            // same way here, against the signature.
            if let ast::ExprKind::Lit(l) = &def.body.kind {
                let lifted = match self.resolve(a) {
                    Ty::Con("expr", args) if args.len() == 3 => args[2].clone(),
                    other => other,
                };
                self.uses.retain(|(id, ..)| *id != def.body.id);
                self.uses
                    .push((def.body.id, def.body.span, lifted, Some(lit_name(l))));
            }
        }
        self.solve()?;
        let t = ann.unwrap_or(t);
        // A member of an overload set is picked by its signature alone, so
        // no use can fill holes of its own: it resolves them like a query.
        let candidate = matches!(
            self.env.scope.get(&def.name),
            Some(Binding::Overloads(module, _)) if *module == self.module
        );
        if candidate || matches!(self.resolve(&t), Ty::Con("query", _)) {
            // A query is evaluated, so its overloads must be resolved:
            // default leftover literals, then report what is still open.
            self.default_lits();
            self.solve()?;
            for (c, sp) in self.pending.clone() {
                if let Cons::Overload {
                    name,
                    module,
                    cands,
                    target,
                    ..
                } = c
                {
                    let fits = self.fitting(module, &cands, &target);
                    let shown: Vec<String> =
                        fits.iter().map(|&i| self.cand_shown(module, i)).collect();
                    let msg = format!(
                        "ambiguous use of `{}` at type {}; candidates: {}",
                        op_name(&name),
                        self.show(&target),
                        shown.join(", ")
                    );
                    return Err(TyErr { span: sp, msg });
                }
            }
        } else {
            // A non-query definition is not evaluated, so its overloads may
            // stay open for its users. One that no candidate can satisfy is an
            // error already, though: without this it stays hidden until
            // something uses the definition (`.a + "x"`). Leftover literals
            // stay polymorphic (`.age >= 18` keeps its type), so this only
            // reports, and never narrows, what the definition inferred.
            if let Some((
                Cons::Overload {
                    name,
                    module,
                    cands,
                    target,
                    ..
                },
                sp,
            )) = self.first_unsatisfiable()
            {
                let shown: Vec<String> =
                    cands.iter().map(|&i| self.cand_shown(module, i)).collect();
                let msg = format!(
                    "no overload of `{}` matches {}; candidates: {}",
                    op_name(&name),
                    self.show(&target),
                    shown.join(", ")
                );
                return Err(TyErr { span: sp, msg });
            }
        }
        Ok(t)
    }

    /// The first open overload with no candidates left once leftover
    /// literals are defaulted. A single trial covers every overload, so the
    /// cost stays linear in their number. Leaves the checker as it found it.
    pub(crate) fn first_unsatisfiable(&mut self) -> Option<(Cons, Span)> {
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let saved = self.pending.clone();
        self.default_lits();
        let _ = self.solve();
        let mut found = None;
        for (c, sp) in &saved {
            if let Cons::Overload {
                module,
                cands,
                target,
                ..
            } = c
            {
                if self.fitting(*module, cands, target).is_empty() {
                    found = Some((c.clone(), *sp));
                    break;
                }
            }
        }
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        self.pending = saved;
        found
    }

    pub(crate) fn default_lits(&mut self) {
        let lits: Vec<(&'static str, Ty)> = self
            .pending
            .iter()
            .filter_map(|(c, _)| match c {
                Cons::Lit { lit, target } => Some((*lit, target.clone())),
                _ => None,
            })
            .collect();
        for (lit, target) in lits {
            if let Ty::Var(_) = self.resolve(&target) {
                let _ = self.unify(&target, &con(lit));
            }
        }
    }

    /// Convert a signature. Its type variables are rigid; its phase variable
    /// is flexible (a plain `expr` may be used at any phase).
    pub(crate) fn annotation(&mut self, t: &TypeExpr) -> Result<Ty, String> {
        let mut res = t;
        let mut args = Vec::new();
        while let TypeExpr::Fun(a, r) = res {
            args.push(a.as_ref());
            res = r;
        }
        if matches!(res, TypeExpr::App { head, .. } if head == "agg" || head == "win")
            && args.iter().any(|a| contains_nullable_expr(a))
        {
            return Err("aggregate/window inputs cannot use `maybe`; use `coalesce` first".into());
        }
        // A plain `expr` result takes the phase of an `agg` / `win`
        // argument (a scalar template over an aggregate is an aggregate).
        let mut wrapped = Vec::new();
        let mut arg = t;
        while let TypeExpr::Fun(a, r) = arg {
            if let TypeExpr::App { head, .. } = a.as_ref() {
                if (head == "agg" || head == "win") && !wrapped.contains(&head.as_str()) {
                    wrapped.push(head.as_str());
                }
            }
            arg = r;
        }
        let phase = match res {
            TypeExpr::App { head, .. } if head == "agg" || head == "win" => con("row"),
            _ => match wrapped.as_slice() {
                // A plain `expr` parameter is phase-polymorphic over row and
                // aggregate (so `_+_` works on aggregates), but never over
                // `win`: a window is the whole of a field, so it is not an
                // argument to anything.
                [] => self.fresh(),
                ["agg"] => con("agg"),
                ["win"] => con("win"),
                _ => return Err(rules::clash(Phase::Agg, Phase::Win)),
            },
        };
        let mut names = HashMap::new();
        self.conv(t, &phase, &mut names)
    }

    pub(crate) fn conv(
        &mut self,
        t: &TypeExpr,
        phase: &Ty,
        names: &mut HashMap<String, Ty>,
    ) -> Result<Ty, String> {
        match t {
            TypeExpr::App { head, args, .. } => {
                let Some(&(name, arity)) = CONS.iter().find(|(n, _)| *n == head.as_str()) else {
                    // `row s` marks a *record of columns* in expression
                    // position, as in `select : expr r (row s) -> ...`. It is a
                    // one-argument wrapper whose argument is a Row, and it is
                    // erased on conversion — `row s` becomes the row `s`, so the
                    // same variable can also appear in `query s`. It is handled
                    // here rather than in `CONS` because `row` is also a
                    // zero-argument internal phase marker (`con("row")`), and
                    // registering it as a constructor would shadow that.
                    if head == "row" {
                        if args.len() != 1 {
                            return Err("`row` takes one type argument".into());
                        }
                        return self.conv_with_kind(&args[0], phase, names, Kind::Row);
                    }
                    if args.is_empty() {
                        return Ok(self.rigid(head, names));
                    }
                    return Err(format!("unknown type constructor `{head}`"));
                };
                if args.len() != arity {
                    return Err(format!(
                        "`{name}` takes {arity} type argument(s), got {}",
                        args.len()
                    ));
                }
                match name {
                    "expr" => {
                        let r = self.conv(&args[0], phase, names)?;
                        // `expr r (row s)` is the public spelling for a
                        // record of column expressions used by select/update.
                        // The aggregate stage wraps this expression in
                        // `agg`, handled below. Keep the field-expression row
                        // separate from the output row so the stage
                        // constraint can validate each expression and compute
                        // its value type.
                        if let TypeExpr::App {
                            head,
                            args: row_args,
                            ..
                        } = &args[1]
                        {
                            if head == "row" {
                                if row_args.len() != 1 {
                                    return Err("`row` takes one type argument".into());
                                }
                                let output =
                                    self.conv_with_kind(&row_args[0], phase, names, Kind::Row)?;
                                return Ok(record_expr(phase.clone(), r, self.fresh_row(), output));
                            }
                        }
                        let a = self.conv(&args[1], phase, names)?;
                        Ok(expr(phase.clone(), r, a))
                    }
                    // `keyMap m r` is the row term for applying mapper `m` to
                    // row `r`. Both arguments are kinded: `m` is `KeyMap`, `r`
                    // is `Row`, and the result is a `Row`.
                    "keyMap" => {
                        let m = self.conv_keymap(&args[0], names)?;
                        if kind_of(&m, &self.vars) != Kind::KeyMap {
                            return Err(
                                "kind mismatch: `keyMap` expects a mapper of kind `KeyMap`".into(),
                            );
                        }
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        // R-KeyMap-Closed applies here too: a `keyMap` written
                        // in a signature with a known row reduces at once, so
                        // `keyMap (prefix "u_") { id = int }` reads as the
                        // renamed row rather than staying symbolic.
                        match reduce_mapkey(&m, &r) {
                            Some(reduced) => Ok(reduced),
                            None => Ok(Ty::MapKey(Box::new(m), Box::new(r))),
                        }
                    }
                    // `merge r s` — right wins on a name collision, and names
                    // only in `s` are appended. A row former, like `Ty::Merge`.
                    "merge" => {
                        let l = self.conv_with_kind(&args[0], phase, names, Kind::Row)?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        Ok(Ty::Merge(Box::new(l), Box::new(r)))
                    }
                    // `removeKey k r` — drop the column named by the label `k`
                    // from row `r`. The label is a type (a `symbol k` variable
                    // or a `Sym` literal), so the signature links `k` to the
                    // result. It reduces at once when both are known.
                    "removeKey" => {
                        let k = self.conv(&args[0], phase, names)?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        match reduce_removekey(&k, &r) {
                            Some(reduced) => Ok(reduced),
                            None => Ok(Ty::RemoveKey(Box::new(k), Box::new(r))),
                        }
                    }
                    // `mapValue w r` — wrap every field type of `r` with the
                    // wrapper `w` names. The witness is a *label*: a closed
                    // `(AsNullable)` names one, and a bare variable is the
                    // `valueLabel w` a signature declared, so the operation
                    // travels in the type exactly as `keyMap`'s mapper does.
                    "mapValue" => {
                        let w = self.conv_valuemap_label(&args[0], names)?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        Ok(Ty::MapValue(Box::new(w), Box::new(r)))
                    }
                    // `keymapper m` introduces a `KeyMap`-kinded variable `m`,
                    // for a signature that takes a mapper as an argument:
                    //   keyMap : keymapper m -> query r -> query (keyMap m r)
                    "keymapper" | "valuemapper" => {
                        let TypeExpr::App { head, args, .. } = &args[0] else {
                            return Err(format!("`{name}` needs a type variable"));
                        };
                        if !args.is_empty() {
                            return Err(format!("`{name}` needs a bare type variable"));
                        }
                        let kind = if name == "keymapper" {
                            Kind::KeyMap
                        } else {
                            Kind::ValueMap
                        };
                        Ok(self.rigid_with_kind(head, names, kind))
                    }
                    // `symbol k` — a column label. It is carried as a `keyLabel`
                    // marker around a `Type` variable, so `infer_app` can
                    // recognise a key parameter and read the stage off the
                    // result. The call site binds the variable to the literal's
                    // `Sym`.
                    "symbol" => {
                        let TypeExpr::App { head, args, .. } = &args[0] else {
                            return Err("`symbol` needs a type variable".into());
                        };
                        if !args.is_empty() {
                            return Err("`symbol` needs a bare type variable".into());
                        }
                        let k = self.rigid(head, names);
                        Ok(Ty::Con("keyLabel", vec![k]))
                    }
                    "agg" | "win" => match &args[0] {
                        TypeExpr::App {
                            head, args: inner, ..
                        } if head == "expr" && inner.len() == 2 => {
                            let r = self.conv(&inner[0], phase, names)?;
                            // The aggregate stage takes a record of aggregate
                            // expressions, so `agg (expr r (row s))` must keep
                            // the record representation used by the stage
                            // constraint while carrying the aggregate phase.
                            if name == "agg" {
                                if let TypeExpr::App {
                                    head: row_head,
                                    args: row_args,
                                    ..
                                } = &inner[1]
                                {
                                    if row_head == "row" {
                                        if row_args.len() != 1 {
                                            return Err("`row` takes one type argument".into());
                                        }
                                        let output = self.conv_with_kind(
                                            &row_args[0],
                                            phase,
                                            names,
                                            Kind::Row,
                                        )?;
                                        return Ok(record_expr(
                                            con(name),
                                            r,
                                            self.fresh_row(),
                                            output,
                                        ));
                                    }
                                }
                            }
                            let a = self.conv(&inner[1], phase, names)?;
                            Ok(expr(con(name), r, a))
                        }
                        _ => Err(format!(
                            "`{name}` wraps an expression type, e.g. `{name} (expr r int)`"
                        )),
                    },
                    _ => {
                        let expected_kinds = expected_arg_kinds(name);
                        let args = args
                            .iter()
                            .enumerate()
                            .map(|(i, a)| {
                                let kind = expected_kinds.get(i).copied().unwrap_or(Kind::Type);
                                self.conv_with_kind(a, phase, names, kind)
                            })
                            .collect::<Result<_, _>>()?;
                        Ok(Ty::Con(name, args))
                    }
                }
            }
            TypeExpr::Record { fields, tail, .. } => {
                let mut fs: Vec<(String, Ty)> = Vec::new();
                for (k, v) in fields {
                    if fs.iter().any(|(o, _)| o == k) {
                        return Err(format!("field `{k}` appears twice in a record type"));
                    }
                    let v = self.conv(v, phase, names)?;
                    fs.push((k.clone(), v));
                }
                let tail = match tail {
                    Some(n) => self.rigid_with_kind(n, names, Kind::Row),
                    None => Ty::Empty,
                };
                Ok(row_or_tail(fs, tail))
            }
            TypeExpr::Fun(a, b) => Ok(fun(
                self.conv(a, phase, names)?,
                self.conv(b, phase, names)?,
            )),
            // A string is only meaningful as a key mapper's affix, which
            // `conv_keymap` handles; anywhere else it is a type-position
            // mistake rather than something to accept silently.
            TypeExpr::Str(_, sp) => Err(format!(
                "a string is not a type here; it is only used as a key mapper's affix, \
                 as in `keyMap (prefix \"u_\") r` (at byte {})",
                sp.start
            )),
            TypeExpr::Error(_) => Ok(self.fresh()),
        }
    }

    pub(crate) fn conv_with_kind(
        &mut self,
        t: &TypeExpr,
        phase: &Ty,
        names: &mut HashMap<String, Ty>,
        expected_kind: Kind,
    ) -> Result<Ty, String> {
        // Mapper arguments use their own surface grammar (`id`, `prefix
        // "..."`, `suffix "..."`, `prependSym k`, `compose ...`), rather than
        // ordinary type constructors, so they are converted by their own
        // routine.
        if expected_kind == Kind::KeyMap {
            return self.conv_keymap(t, names);
        }
        match t {
            TypeExpr::App { head, args, .. } => {
                // `row s` is the record-in-expression-position marker; its
                // argument is a Row regardless of the kind expected here. See
                // the same case in `conv`.
                if head == "row" {
                    if args.len() != 1 {
                        return Err("`row` takes one type argument".into());
                    }
                    return self.conv_with_kind(&args[0], phase, names, Kind::Row);
                }
                let Some(&(_name, _arity)) = CONS.iter().find(|(n, _)| *n == head.as_str()) else {
                    if args.is_empty() {
                        return Ok(self.rigid_with_kind(head, names, expected_kind));
                    }
                    return Err(format!("unknown type constructor `{head}`"));
                };
                // If it's a known constructor, use regular conv
                self.conv(t, phase, names)
            }
            TypeExpr::Record { fields, tail, .. } => {
                // Records are always rows, regardless of expected_kind
                let mut fs: Vec<(String, Ty)> = Vec::new();
                for (k, v) in fields {
                    if fs.iter().any(|(o, _)| o == k) {
                        return Err(format!("field `{k}` appears twice in a record type"));
                    }
                    let v = self.conv(v, phase, names)?;
                    fs.push((k.clone(), v));
                }
                let tail = match tail {
                    Some(n) => self.rigid_with_kind(n, names, Kind::Row),
                    None => Ty::Empty,
                };
                Ok(row_or_tail(fs, tail))
            }
            TypeExpr::Fun(_, _) | TypeExpr::Error(_) | TypeExpr::Str(_, _) => {
                self.conv(t, phase, names)
            }
        }
    }

    /// Convert a `mapValue` witness to the label the row term carries.
    ///
    /// A closed wrapper names its label (`(AsNullable)` is `"nullable"`), and a
    /// bare type variable is the `valueLabel w` a signature declared — the same
    /// variable in the parameter and in the result row, which is what lets the
    /// operation travel in the type instead of being read at each application.
    pub(crate) fn conv_valuemap_label(
        &mut self,
        t: &TypeExpr,
        names: &mut HashMap<String, Ty>,
    ) -> Result<Ty, String> {
        match t {
            TypeExpr::App { head, args, .. } if args.is_empty() => match head.as_str() {
                "AsNullable" => Ok(Ty::Sym("nullable".to_string())),
                "AsList" => Ok(Ty::Sym("list".to_string())),
                "Id" => Ok(Ty::Sym("id".to_string())),
                _ => Ok(self.rigid(head, names)),
            },
            _ => Err(
                "expected a value wrapper, such as `(AsNullable)`, or a `valueLabel` variable"
                    .into(),
            ),
        }
    }

    pub(crate) fn rigid(&mut self, name: &str, names: &mut HashMap<String, Ty>) -> Ty {
        self.rigid_with_kind(name, names, Kind::Type)
    }

    /// Convert a `KeyMap`-kinded type symbol: `(prefix "u_")`, `(suffix "s")`,
    /// `(compose f g)`, `id`, or a bare *variable* of kind `KeyMap`.
    ///
    /// The result is a `Ty`, not a closed `KeyMap`, because a signature may be
    /// polymorphic in the mapping: `keyMap m r` with `m` a variable is how a
    /// helper such as `addPrefix = p => q => q & prefix p` is typed. The term
    /// is retained with the variable in place and reduces once that variable is
    /// bound to a constructor.
    ///
    /// A concrete `prefix`/`suffix` mapper needs a **literal** affix: it decides the
    /// output row's labels, so there has to be something to apply. A variable
    /// is accepted precisely because it is deferred rather than applied.
    pub(crate) fn conv_keymap(
        &mut self,
        t: &TypeExpr,
        names: &mut HashMap<String, Ty>,
    ) -> Result<Ty, String> {
        let TypeExpr::App { head, args, .. } = t else {
            return Err("expected a key mapper, such as `(prefix \"u_\")`".into());
        };
        match head.as_str() {
            "id" => {
                if !args.is_empty() {
                    return Err("`id` takes no type argument".into());
                }
                Ok(KeyMap::Id.to_ty())
            }
            "prefix" | "suffix" => {
                let [arg] = args.as_slice() else {
                    return Err(format!("`{head}` takes one type argument, the affix"));
                };
                // A variable affix cannot be a witness: the affix decides the
                // output labels, so it must be known here.
                let affix = string_literal_of(arg).ok_or_else(|| {
                    format!(
                        "`{head}` needs a string literal affix, such as `({head} \"u_\")`: \
                         the mapping is part of the type, so it must be known when the \
                         query is checked"
                    )
                })?;
                let km = if head == "prefix" {
                    KeyMap::Prefix(affix)
                } else {
                    KeyMap::Suffix(affix)
                };
                Ok(km.to_ty())
            }
            // `prependSym k` / `appendSym k` — a prefix or suffix mapper whose
            // label is a `symbol k` variable. It stays open until the call site
            // binds `k` to a literal, then `resolve_mapper` turns it into the
            // concrete `KeyAffix`. Unlike `prefix "x"`, no literal is needed.
            "prependSym" | "appendSym" => {
                let [label] = args.as_slice() else {
                    return Err(format!("`{head}` takes one symbol argument"));
                };
                let TypeExpr::App {
                    head: lh, args: la, ..
                } = label
                else {
                    return Err(format!("`{head}` needs a symbol variable"));
                };
                if !la.is_empty() {
                    return Err(format!("`{head}` needs a bare symbol variable"));
                }
                let k = self.rigid(lh, names);
                let tag = if head == "prependSym" {
                    "labelPrefix"
                } else {
                    "labelSuffix"
                };
                Ok(Ty::Con(tag, vec![k]))
            }
            "compose" => {
                let [f, g] = args.as_slice() else {
                    return Err("`compose` takes two key mappers".into());
                };
                // Fusing two *concrete* mappers is sound and cheap; if either is
                // a variable the composition stays symbolic.
                let f = self.conv_keymap(f, names)?;
                let g = self.conv_keymap(g, names)?;
                match (KeyMap::of_ty(&f), KeyMap::of_ty(&g)) {
                    (Some(f), Some(g)) => Ok(KeyMap::Compose(Box::new(f), Box::new(g)).to_ty()),
                    _ => Ok(Ty::Con("KeyMapCompose", vec![f, g])),
                }
            }
            other => {
                // The mapper constructors are deliberately lowercase surface
                // forms. Keep the former capitalized spellings out of the
                // fallback for bare mapper variables, so they cannot quietly
                // act as aliases (especially `Id`, which has no arguments).
                if matches!(other, "Id" | "Prefix" | "Suffix" | "Compose") {
                    return Err(format!(
                        "unknown key mapper `{other}`; expected `id`, `(prefix \"s\")`, \
                         `(suffix \"s\")`, `(compose f g)`, or a mapper variable"
                    ));
                }
                if args.is_empty() {
                    // Known type/value constructors are not mapper variables.
                    // Rejecting them here keeps the four kinds disjoint even
                    // though both a mapper variable and a type variable are
                    // represented by a bare identifier in the syntax.
                    if CONS
                        .iter()
                        .any(|(name, arity)| *name == other && *arity == 0)
                        || matches!(other, "AsNullable" | "AsList")
                    {
                        return Err(format!("kind mismatch: `{other}` is not a key mapper"));
                    }
                    // A bare variable of kind KeyMap, bound by `keymapper` in a
                    // signature or inferred at a use. This is the deferred case:
                    // it carries no transformation yet.
                    return Ok(self.rigid_with_kind(other, names, Kind::KeyMap));
                }
                Err(format!(
                    "unknown key mapper `{other}`; expected `id`, `(prefix \"s\")`, \
                     `(suffix \"s\")`, `(compose f g)`, or a mapper variable"
                ))
            }
        }
    }

    pub(crate) fn rigid_with_kind(
        &mut self,
        name: &str,
        names: &mut HashMap<String, Ty>,
        kind: Kind,
    ) -> Ty {
        if let Some(t) = names.get(name) {
            return t.clone();
        }
        let t = Ty::Rigid(self.fresh_with(false, true, kind), name.to_string());
        names.insert(name.to_string(), t.clone());
        t
    }

    /// Fresh copy of a scheme; its deferred constraints join the pending set.
    /// Its holes become overload uses at `site`. Fails when the definition
    /// leaves more open overloads than an encoded origin can hold.
    pub(crate) fn instantiate(
        &mut self,
        s: &Scheme,
        span: Span,
        site: Option<u32>,
    ) -> Result<Ty, String> {
        let mut map = HashMap::new();
        let t = self.inst(&s.ty, &mut map, &s.gens);
        for c in &s.cons {
            let mut c = c.map(&mut |t| self.inst(t, &mut map, &s.gens));
            if let (Cons::Overload { origin, .. }, Some(site)) = (&mut c, site) {
                if let Origin::Hole(k) = *origin {
                    // Resolved as hole `k` of the definition used at `site`.
                    let encoded = encode(site, k).ok_or_else(|| {
                        "too many open overloads: this definition is used where it would need more \
                         than 256 unresolved overloaded names; give it a type signature"
                            .to_string()
                    })?;
                    *origin = Origin::Site(encoded);
                }
            }
            self.pending.push((c, span));
        }
        Ok(t)
    }

    /// Fresh arena variables for a scheme's `Gen` variables.
    pub(crate) fn inst(&mut self, t: &Ty, map: &mut HashMap<u32, Ty>, gens: &[GenInfo]) -> Ty {
        match self.resolve(t) {
            Ty::Gen(k) => {
                if let Some(t) = map.get(&k) {
                    return t.clone();
                }
                let g = &gens[k as usize];
                let n = Ty::Var(self.fresh_with(g.row_or_win, g.nonnull, g.kind));
                map.insert(k, n.clone());
                n
            }
            Ty::KeyAffix(n, s) => Ty::KeyAffix(n, s.clone()),
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.inst(a, map, gens)).collect()),
            Ty::Fun(a, b) => fun(self.inst(&a, map, gens), self.inst(&b, map, gens)),
            Ty::Row(fs, tail) => {
                let fs = fs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.inst(v, map, gens)))
                    .collect();
                row(fs, self.inst(&tail, map, gens))
            }
            // The mapper is instantiated too: a use of a deferred `prefix p` gets
            // its own mapper variable, which the affix argument then binds.
            Ty::MapKey(km, row) => Ty::MapKey(
                Box::new(self.inst(&km, map, gens)),
                Box::new(self.inst(&row, map, gens)),
            ),
            Ty::Merge(left, right) => Ty::Merge(
                Box::new(self.inst(&left, map, gens)),
                Box::new(self.inst(&right, map, gens)),
            ),
            Ty::MapValue(label, row) => Ty::MapValue(
                Box::new(self.inst(&label, map, gens)),
                Box::new(self.inst(&row, map, gens)),
            ),
            Ty::RemoveKey(k, row) => Ty::RemoveKey(
                Box::new(self.inst(&k, map, gens)),
                Box::new(self.inst(&row, map, gens)),
            ),
            t @ (Ty::Var(_) | Ty::Rigid(..) | Ty::Empty | Ty::Sym(_)) => t,
        }
    }

    /// Close a type and its constraints into a self-contained scheme: every
    /// free variable becomes a `Gen` index carrying its flags, so the scheme
    /// no longer refers to this checker's variable arena.
    pub(crate) fn generalize(&self, ty: &Ty, cons: Vec<Cons>) -> Scheme {
        let (mut map, mut gens) = (HashMap::new(), Vec::new());
        let ty = self.gen(ty, &mut map, &mut gens);
        let mut out = Vec::new();
        for c in &cons {
            out.push(c.map(&mut |t| self.gen(t, &mut map, &mut gens)));
        }
        Scheme {
            ty,
            cons: out,
            gens,
            stage: None,
            failed: false,
        }
    }

    /// The stage a definition is, from its body. A `primitive` whose signature
    /// is a query stage owns that stage's row rule.
    pub(crate) fn def_stage(&self, i: usize) -> Option<RowStage> {
        match &self.env.defs[i].body.kind {
            ExprKind::Primitive(name) => {
                crate::primitive::prim_of(name).and_then(RowStage::of_prim)
            }
            _ => None,
        }
    }

    pub(crate) fn gen(&self, t: &Ty, map: &mut HashMap<u32, u32>, gens: &mut Vec<GenInfo>) -> Ty {
        match self.resolve(t) {
            r @ (Ty::Var(_) | Ty::Rigid(..)) => {
                let (v, name) = match r {
                    Ty::Var(v) => (v, None),
                    Ty::Rigid(v, n) => (v, Some(n)),
                    _ => unreachable!(),
                };
                let k = *map.entry(v).or_insert_with(|| {
                    let info = &self.vars[v as usize];
                    gens.push(GenInfo {
                        kind: info.kind,
                        row_or_win: info.row_or_win,
                        nonnull: info.nonnull,
                        name,
                    });
                    gens.len() as u32 - 1
                });
                Ty::Gen(k)
            }
            Ty::KeyAffix(n, s) => Ty::KeyAffix(n, s.clone()),
            Ty::Sym(s) => Ty::Sym(s),
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.gen(a, map, gens)).collect()),
            Ty::Fun(a, b) => fun(self.gen(&a, map, gens), self.gen(&b, map, gens)),
            Ty::Row(fs, tail) => {
                let fs = fs
                    .iter()
                    .map(|(k, v)| (k.clone(), self.gen(v, map, gens)))
                    .collect();
                row(fs, self.gen(&tail, map, gens))
            }
            // The mapper is generalized too: it may be a variable (a deferred
            // `prefix p`), and quantifying it is what gives each use its own
            // fresh mapper that the affix argument can then bind.
            Ty::MapKey(km, row) => Ty::MapKey(
                Box::new(self.gen(&km, map, gens)),
                Box::new(self.gen(&row, map, gens)),
            ),
            Ty::Merge(left, right) => Ty::Merge(
                Box::new(self.gen(&left, map, gens)),
                Box::new(self.gen(&right, map, gens)),
            ),
            Ty::MapValue(label, row) => Ty::MapValue(
                Box::new(self.gen(&label, map, gens)),
                Box::new(self.gen(&row, map, gens)),
            ),
            Ty::RemoveKey(k, row) => Ty::RemoveKey(
                Box::new(self.gen(&k, map, gens)),
                Box::new(self.gen(&row, map, gens)),
            ),
            t @ (Ty::Gen(_) | Ty::Empty) => t,
        }
    }

    // ── inference ──────────────────────────────────────────────────────────

    pub(crate) fn infer(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        match &e.kind {
            ExprKind::Name(..) => self.infer_name(env, e),
            ExprKind::Lit(..) => self.infer_lit(env, e),
            ExprKind::Field(..) => self.infer_field(env, e),
            ExprKind::Proj(..) => self.infer_proj(env, e),
            ExprKind::App(..) => self.infer_app(env, e),
            ExprKind::Lambda(..) => self.infer_lambda(env, e),
            ExprKind::Record(..) => self.infer_record(env, e),
            ExprKind::List(..) => self.infer_list(env, e),
            ExprKind::Sql(..) => self.infer_sql(env, e),
            ExprKind::Case(..) => self.infer_case(env, e),
            ExprKind::Primitive(..) => self.infer_primitive(env, e),
            ExprKind::Error => self.infer_error(env, e),
        }
    }

    /// Check `e` **against an expected type**, propagating that expectation
    /// downward instead of recovering it by unification after the fact.
    ///
    /// This is the checking half of bidirectional typing. [`Self::infer`]
    /// synthesizes a type bottom-up and the caller then unifies it with what it
    /// wanted ([`Self::coerce`]); `check` supplies the wanted type top-down, so
    /// a form that cannot be typed without knowing its result — a record
    /// literal whose field types come from an expected row, a lambda whose
    /// parameter type comes from an expected function — is typed directly
    /// rather than left open for the constraint solver to guess at.
    ///
    /// The default is to synthesize and then coerce, which is always sound.
    /// The forms that override it are the ones where the expectation carries
    /// information the expression cannot produce on its own.
    ///
    /// Each subexpression is visited **exactly once** on every path. That is
    /// load-bearing, not stylistic: the type of every expression is recorded
    /// under its `ExprId`, and the typed tree — which elaboration and the
    /// incremental facts both read — is built from those records. Synthesizing
    /// a node and *then* re-visiting its children to check them would record
    /// each child twice.
    pub(crate) fn check(
        &mut self,
        env: &mut Vec<(String, Ty)>,
        e: &ast::Expr,
        expected: &Ty,
    ) -> R<Ty> {
        match &e.kind {
            // A record literal is the surface argument of a `row`-typed
            // position: `expr r (row s)`, `agg (expr r (row s))`, a `winspec`.
            // Its fields are named, so the expected row supplies each field's
            // type rather than each field being unified against a fresh
            // variable.
            ExprKind::Record(..) => self.check_record(env, e, expected),
            // A lambda's parameter type comes from the expected function type
            // when it has one.
            ExprKind::Lambda(..) => self.check_lambda(env, e, expected),
            // A list literal's element type comes from the expected `list t`.
            ExprKind::List(..) => self.check_list(env, e, expected),
            // A string literal checked against a `symbol k` label parameter
            // binds `k` to the literal's `Sym`. Only a `keyLabel` expectation
            // takes this path; every other string keeps its `string` type.
            ExprKind::Lit(ast::Lit::Str(s))
                if matches!(
                    self.resolve(expected),
                    Ty::Con("keyLabel" | "valueLabel", ref a) if a.len() == 1
                ) =>
            {
                let Ty::Con(_, inner) = self.resolve(expected) else {
                    unreachable!("matched a label expectation")
                };
                self.unify(&inner[0], &Ty::Sym(s.clone()))
                    .map_err(at(e.span))?;
                Ok(Ty::Sym(s.clone()))
            }
            _ => {
                let actual = self.infer(env, e)?;
                self.coerce(&actual, expected).map_err(at(e.span))?;
                // A literal is **polymorphic**, closer to `fromInteger` and
                // `fromString` than to `int` and `string`: it takes whatever
                // numeric or string-like type its context demands, so `1` in a
                // float position *is* a float. `infer_lit` can only record the
                // literal's default kind, so a literal checked against a known
                // type has to be re-recorded at the type it was lifted to.
                //
                // Without this, `1` reads back as `int` in a `case` typed
                // `float`, and the `case` claims a type one of its own forks does
                // not have. Measured: the `1` fork recorded `Int` while the case
                // recorded `float`. The application path re-records a literal
                // argument for the same reason; this generalises it to every
                // checked position, which is where a `case` arm is.
                if let ExprKind::Lit(l) = &e.kind {
                    let lifted = match self.resolve(expected) {
                        Ty::Con("expr", a) if a.len() == 3 => a[2].clone(),
                        other => other,
                    };
                    self.uses.push((e.id, e.span, lifted, Some(lit_name(l))));
                }
                Ok(self.resolve(expected))
            }
        }
    }

    /// Check `e` against `expected`, discarding the resulting type.
    ///
    /// The common case: a caller that knows what it wants and only needs to
    /// know whether it got it.
    pub(crate) fn expect(
        &mut self,
        env: &mut Vec<(String, Ty)>,
        e: &ast::Expr,
        expected: &Ty,
    ) -> R<()> {
        self.check(env, e, expected).map(|_| ())
    }

    /// A record literal checked against an expected row.
    ///
    /// Each field is checked against the type the expected row gives it. A
    /// field the expectation does not mention is checked against a fresh
    /// variable, so an over-long record is still reported by the constraint
    /// that compares the two rows rather than here.
    fn check_record(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr, expected: &Ty) -> R<Ty> {
        let ExprKind::Record(fields) = &e.kind else {
            unreachable!("dispatched on ExprKind::Record")
        };
        let want = self.resolve(expected);
        // Only the fields the expectation already names can type anything. An
        // open row may still grow, so it supplies nothing here; the remaining
        // fields fall back to fresh variables and the row comparison settles
        // them.
        let (expected_fields, _) = self.flatten(&want);
        let mut typed: Vec<(String, Ty)> = Vec::new();
        for (name, value) in fields {
            // The duplicate-field rule is `infer_record`'s. This path visits
            // the fields itself, so it must apply the same rule: reporting it
            // in only one direction would make the diagnostic depend on which
            // way the type flowed.
            if typed.iter().any(|(n, _)| n == name) {
                return Err(TyErr {
                    span: e.span,
                    msg: format!("field `{name}` appears twice"),
                });
            }
            let field_ty = expected_fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t.clone())
                .unwrap_or_else(|| self.fresh());
            self.check(env, value, &field_ty)?;
            typed.push((name.clone(), field_ty));
        }
        // Each field was visited exactly once, above. The record's own type is
        // assembled from those results rather than re-inferred, because
        // inferring the whole record would walk every field a second time.
        let actual = row(typed, Ty::Empty);
        self.coerce(&actual, expected).map_err(at(e.span))?;
        Ok(self.resolve(expected))
    }

    /// A lambda checked against an expected function type.
    ///
    /// The parameter type comes from the expectation instead of a fresh
    /// variable, so the body is inferred in an environment that already knows
    /// it. That direction is the point: `infer_lambda` binds each parameter to
    /// a fresh variable and lets the body's unifications decide, which is sound
    /// but loses the expectation — and is why annotating a helper's signature
    /// used to be the workaround for a deferred row.
    ///
    /// The body is inferred exactly once and its type *is* the result type of
    /// the function type returned; inferring the whole lambda as well would
    /// visit every subexpression twice.
    fn check_lambda(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr, expected: &Ty) -> R<Ty> {
        let ExprKind::Lambda(param, body) = &e.kind else {
            unreachable!("dispatched on ExprKind::Lambda")
        };
        let want = self.resolve(expected);
        // Peel one arrow when the expectation has one. When it is not a
        // function there is nothing to draw the parameter type from, so
        // synthesis is the only option.
        let param_ty = match &want {
            Ty::Fun(p, _) => Some((**p).clone()),
            _ => None,
        };
        let Some(param_ty) = param_ty else {
            let actual = self.infer(env, e)?;
            self.coerce(&actual, expected).map_err(at(e.span))?;
            return Ok(self.resolve(expected));
        };
        env.push((param.clone(), param_ty.clone()));
        let body_ty = self.infer(env, body);
        env.pop();
        let actual = fun(param_ty, body_ty?);
        self.coerce(&actual, expected).map_err(at(e.span))?;
        Ok(self.resolve(expected))
    }

    /// A list literal checked against an expected `list t`.
    ///
    /// Each item is visited once, against the expected element type; the list's
    /// own type is assembled from that rather than re-inferred.
    fn check_list(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr, expected: &Ty) -> R<Ty> {
        let ExprKind::List(items) = &e.kind else {
            unreachable!("dispatched on ExprKind::List")
        };
        let want = self.resolve(expected);
        let element = match &want {
            Ty::Con(name, args) if *name == "list" && args.len() == 1 => Some(args[0].clone()),
            _ => None,
        };
        let Some(element) = element else {
            let actual = self.infer(env, e)?;
            self.coerce(&actual, expected).map_err(at(e.span))?;
            return Ok(self.resolve(expected));
        };
        for item in items {
            self.check(env, item, &element)?;
        }
        let actual = Ty::Con("list", vec![element]);
        self.coerce(&actual, expected).map_err(at(e.span))?;
        Ok(self.resolve(expected))
    }

    fn infer_name(&mut self, env: &mut [(String, Ty)], e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Name(n) = &e.kind else {
            unreachable!("dispatched on ExprKind::Name")
        };
        let t = match env.iter().rev().find(|(k, _)| k == n) {
            Some((_, t)) => t.clone(),
            None => self.lookup(n, e.id, sp).map_err(at(sp))?,
        };
        self.uses.push((e.id, sp, t.clone(), None));
        Ok(t)
    }

    fn infer_lit(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Lit(l) = &e.kind else {
            unreachable!("dispatched on ExprKind::Lit")
        };
        let t = con(lit_name(l));
        self.uses.push((e.id, sp, t.clone(), None));
        Ok(t)
    }

    fn infer_field(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Field(side, n) = &e.kind else {
            unreachable!("dispatched on ExprKind::Field")
        };
        let phase = self.fresh_col_phase();
        let (tail, a) = (self.fresh_row(), self.fresh());
        let r = if n == PROBE_FIELD {
            // Completion probe: adds no column, remembers the row and
            // which definition asked, so a later definition's empty
            // probe cannot overwrite this one.
            let owner = self.active.last().map(|(_, d)| *d).unwrap_or(usize::MAX);
            self.probe = Some((owner, tail.clone()));
            tail
        } else {
            row(vec![(n.clone(), a.clone())], tail)
        };
        let input = match side {
            Side::Single => r,
            Side::Left => Ty::Con("join", vec![r, self.fresh_row()]),
            Side::Right => Ty::Con("join", vec![self.fresh_row(), r]),
        };
        // Hover shows the column's value type, not the whole row.
        self.uses.push((e.id, sp, a.clone(), None));
        Ok(expr(phase, input, a))
    }

    fn infer_proj(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Proj(base, f) = &e.kind else {
            unreachable!("dispatched on ExprKind::Proj")
        };
        if let ExprKind::Name(n) = &base.kind {
            let scope = self.env.scope;
            if !env.iter().any(|(k, _)| k == n) {
                if let Some(Binding::Module(t)) = scope.get(n) {
                    let binding = self.env.owns[t].get(f).cloned();
                    // Record **what this projection resolved to**, exactly as
                    // `lookup` does for a bare name. A module-scope name becomes
                    // a binding in two places, and leaving this one unrecorded
                    // is what made `lib.shout .name` a construct with no term
                    // rule: the fact the bridge reads was never written for a
                    // qualified name. One place records, so the two cannot
                    // disagree.
                    if let Some(resolved) = binding.as_ref().and_then(Resolved::of_binding) {
                        self.resolutions.insert((self.module, e.id), resolved);
                    }
                    let t = match binding {
                        Some(Binding::Def(dm, i)) => {
                            self.def_type(dm, i, e.id, sp).map_err(at(sp))?
                        }
                        Some(Binding::Overloads(om, is)) => {
                            self.overload_type(f, om, &is, e.id, sp)
                        }
                        _ => {
                            return Err(TyErr {
                                span: sp,
                                msg: format!("module `{n}` has no definition `{f}`"),
                            })
                        }
                    };
                    self.uses.push((e.id, sp, t.clone(), None));
                    return Ok(t);
                }
            }
        }
        let bt = self.infer(env, base)?;
        let (a, tail) = (self.fresh(), self.fresh());
        let want = row(vec![(f.clone(), a.clone())], tail);
        self.unify(&bt, &want)
            .map_err(|msg| format!("cannot take `.{f}`: {msg}"))
            .map_err(at(sp))?;
        Ok(a)
    }

    fn infer_app(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::App(f, args) = &e.kind else {
            unreachable!("dispatched on ExprKind::App")
        };
        let mut ft = self.infer(env, f)?;
        for arg in args {
            if let Ty::Fun(p, r) = self.resolve(&ft) {
                // A key argument (`omit "k"`, `prefix "p"`, `suffix "s"`) names
                // a column or supplies a label fragment, so its value has to be
                // known when the query is checked. The argument itself is
                // checked like any other one — a literal binds the signature's
                // label variable, and a helper's own parameter is a name — but a
                // *computed* key cannot name a column, so it is refused here,
                // where the syntax still is, rather than deferred.
                //
                // Nothing about the stage is recorded here. Its row rule
                // belongs to the stage's own definition (`prim_type`), and every
                // use instantiates that rule with fresh variables; rebuilding
                // the rule here, out of the callee's *result* type, is what made
                // a body with two key stages read the second stage's input as
                // the composed row. See docs/TYPE-SYSTEM-REDESIGN.md.
                // The stage this callee *is*, when its definition owns a row
                // rule. A helper's scheme says nothing, so its use is not a
                // stage application: the stages it applies were recorded in its
                // own body, against its own parameter rows.
                let stage = self.callee_row_stage(f);
                if is_key_parameter(&p) && !is_key_argument(arg) {
                    return Err(TyErr {
                        span: arg.span,
                        msg: key_argument_message(f),
                    });
                }
                if let Some(kind) = stage {
                    // Row stages with a key parameter are recorded at that
                    // witness argument. Expression-bearing stages have no
                    // witness; their first argument leaves the stage with
                    // exactly `query input -> query output`, which is the
                    // same point at which its own row boundary is available.
                    let records_here = if is_key_parameter(&p) {
                        true
                    } else {
                        !matches!(
                            kind,
                            RowStage::Prefix
                                | RowStage::Suffix
                                | RowStage::Omit
                                | RowStage::MapValue
                        ) && matches!(
                            self.resolve(&r),
                            Ty::Fun(ref next, ref result)
                                if matches!(self.resolve(next), Ty::Con("query", _))
                                    && matches!(self.resolve(result), Ty::Con("query", _))
                        )
                    };
                    if records_here {
                        let label = if is_key_parameter(&p) {
                            // The label the signature declared is the one this
                            // argument binds, so the stage's witness is the
                            // same variable its row term mentions.
                            key_label(&p)
                        } else {
                            // Expression stages have no label. The checked
                            // view intentionally turns this non-symbol into
                            // `None`; the value only supplies the stage's
                            // application boundary here.
                            (*p).clone()
                        };
                        self.record_row_stage(kind, e.id, f.span, label, &r);
                    }
                }
            }

            self.span = arg.span;
            // Resolve the callee's type *before* typing the argument, so a
            // known parameter type can be pushed down into the argument instead
            // of being recovered by unifying after the fact.
            //
            // This is the direction that matters most: an application is where
            // a lambda, a record literal, or a list is most often written, and
            // those are exactly the forms that cannot be synthesized without
            // knowing what they are being checked against. Typing the argument
            // first and coercing afterwards still works when the argument is
            // self-describing (`.age`, `1`, `upper`) — which is why this went
            // unnoticed — but leaves the expectation to unification when it is
            // not.
            ft = match self.resolve(&ft) {
                Ty::Fun(p, r) => {
                    let p = *p;
                    // `check` re-records a literal argument at the type it was
                    // lifted to (`18` in `.age >= 18` has the value type it was
                    // lifted to), so no special case is needed here.
                    self.check(env, arg, &p)?;
                    *r
                }
                Ty::Var(_) => {
                    let at_ = self.infer(env, arg)?;
                    let r = self.fresh();
                    self.unify(&ft, &fun(at_, r.clone()))
                        .map_err(at(arg.span))?;
                    r
                }
                o => {
                    // The callee is neither a function nor a variable. Report
                    // that before typing the argument: the argument cannot
                    // change the verdict, and typing it first would add
                    // unrelated diagnostics for a call that was never
                    // well-formed.
                    let msg = format!(
                        "cannot apply a value of type {} to an argument",
                        self.show(&o)
                    );
                    return Err(TyErr {
                        span: arg.span,
                        msg,
                    });
                }
            };
            self.solve()?;
        }
        // Record the **application's own** type after solving, not before.
        //
        // `ft` is only partly determined mid-loop: for `.age + 1` the type is
        // `expr r a` with `a` still an unsolved variable, and it becomes `int`
        // only once `solve` has run the constraints the earlier arguments
        // queued. Recording the pre-solve form put a variable in the map, which
        // a reader sees as `Unknown` — a type that looks recorded and is not
        // usable, which is worse than recording nothing.
        self.solve()?;
        self.uses.push((e.id, e.span, self.resolve(&ft), None));
        Ok(ft)
    }

    fn infer_lambda(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::Lambda(p, body) = &e.kind else {
            unreachable!("dispatched on ExprKind::Lambda")
        };
        let pt = self.fresh();
        env.push((p.clone(), pt.clone()));
        let bt = self.infer(env, body);
        env.pop();
        Ok(fun(pt, bt?))
    }

    fn infer_record(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Record(fs) = &e.kind else {
            unreachable!("dispatched on ExprKind::Record")
        };
        let mut out: Vec<(String, Ty)> = Vec::new();
        for (k, v) in fs {
            if out.iter().any(|(o, _)| o == k) {
                return Err(TyErr {
                    span: sp,
                    msg: format!("field `{k}` appears twice"),
                });
            }
            let t = self.infer(env, v)?;
            out.push((k.clone(), t));
        }
        Ok(row(out, Ty::Empty))
    }

    fn infer_list(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::List(xs) = &e.kind else {
            unreachable!("dispatched on ExprKind::List")
        };
        let mut ts = Vec::new();
        for x in xs {
            ts.push((self.infer(env, x)?, x.span));
        }
        let Some((first, _)) = ts.first() else {
            // An empty list has no element to go on; it fits any.
            return Ok(list(self.fresh()));
        };
        // A list of column expressions is a list of sort / partition
        // keys, so `[asc .x, .y]` has one element type.
        let elem = match self.resolve(first) {
            Ty::Con("expr" | "sortkey", _) => sortkey(self.fresh()),
            _ => first.clone(),
        };
        for (t, s) in &ts {
            self.coerce(t, &elem).map_err(at(*s))?;
        }
        Ok(list(elem))
    }

    fn infer_sql(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Sql(_) = &e.kind else {
            unreachable!("dispatched on ExprKind::Sql")
        };
        Err(TyErr {
            span: sp,
            msg: "`sql \"...\"` must be the whole body of a definition with a type signature"
                .into(),
        })
    }

    /// `case { cond => value; ... }`.
    ///
    /// One shared result type and one row, exactly like `ifThenElse`, whose
    /// type is `expr r bool -> expr r a -> expr r a -> expr r a`: every
    /// condition is a boolean expression over the same row, every value is an
    /// expression over that row, and all the values have one type. The phase
    /// is left free, because an expression type carries its own phase — a case
    /// whose arms are aggregates is an aggregate, the same way
    /// `ifThenElse c (sum .x) 0` is.
    fn infer_case(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Case(arms) = &e.kind else {
            unreachable!("dispatched on ExprKind::Case")
        };
        // An armless `case` has no value to take a type from, so it is refused
        // rather than given a made-up one. Reported here instead of in the
        // parser because `case { }` is well-formed syntax whose problem is
        // that it cannot be typed.
        if arms.is_empty() {
            return Err(TyErr {
                span: sp,
                msg: "`case` needs at least one arm: write `cond => value`, and end with \
                      `true => ...` for a default"
                    .into(),
            });
        }
        // The last arm must be unconditional, so the `case` always produces a
        // value. Without it SQL's `CASE` yields NULL for an unmatched row while
        // the arms present would type the result as non-null — the expression
        // would claim a value it can fail to produce. `ifThenElse` has no such
        // hole because its else is a required argument, and `true => ...` is
        // how the n-ary form writes the same thing.
        match arms.last() {
            Some((cond, _)) if matches!(cond.kind, ExprKind::Lit(ast::Lit::Bool(true))) => {}
            _ => {
                return Err(TyErr {
                    span: sp,
                    msg: "a `case` must end with a default arm, written `true => value`, so \
                          that it always produces a value"
                        .into(),
                })
            }
        }
        // The phase is a plain unconstrained variable, *not*
        // `fresh_col_phase()`: a `case` is phase-polymorphic like
        // `ifThenElse`, whose signature declares `expr r a -> expr r a` with
        // `r` free. `fresh_col_phase` marks the variable row-or-win, and `bind`
        // refuses to let such a variable become `agg` — which is the rule that
        // keeps a bare column out of an aggregate. Using it here made every
        // aggregate arm fail with "aggregates cannot mix with ungrouped
        // columns", even though `ifThenElse (…) (sum .amount) 0.0` is legal.
        let (phase, row) = (self.fresh(), self.fresh_row());
        let result = self.fresh();
        for (cond, value) in arms {
            // Both positions have a type the language fixes in advance — the
            // condition is a boolean expression, the value an expression of the
            // `case`'s own result type — so both are *checked* against it
            // rather than synthesized and unified afterwards.
            self.span = cond.span;
            self.expect(env, cond, &expr(phase.clone(), row.clone(), con("bool")))?;
            self.span = value.span;
            self.expect(
                env,
                value,
                &expr(phase.clone(), row.clone(), result.clone()),
            )?;
        }
        self.uses.push((e.id, sp, result.clone(), None));
        Ok(expr(phase, row, result))
    }

    fn infer_primitive(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        let ExprKind::Primitive(_) = &e.kind else {
            unreachable!("dispatched on ExprKind::Primitive")
        };
        Err(TyErr {
            span: sp,
            msg: "`primitive \"...\"` must be the whole body of a definition with a type signature"
                .into(),
        })
    }

    fn infer_error(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::Error = &e.kind else {
            unreachable!("dispatched on ExprKind::Error")
        };
        Ok(self.fresh())
    }

    pub(crate) fn lookup(&mut self, n: &str, site: u32, sp: Span) -> Result<Ty, String> {
        let binding = self.env.scope.get(n).cloned();
        // Record **what this occurrence resolved to**, keyed by its `ExprId`.
        //
        // This is the fact a consumer working from the tree needs and cannot
        // reconstruct: `use_ty` says what type an expression has, but not which
        // definition a name referred to. Without it, any phase that wants to
        // expand a call has to look the name up again in the scope — and a
        // second lookup is a second answer waiting to disagree with the first.
        // That is the shape of the bugs this project kept producing: an alias
        // followed in checking but not in elaboration, a call the checker took
        // and the elaborator refused.
        //
        // Recorded here because this is the single place a module-scope name
        // becomes a binding; a resolution recorded anywhere else could fall out
        // of step with the one actually used.
        if let Some(resolved) = binding.as_ref().and_then(Resolved::of_binding) {
            self.resolutions.insert((self.module, site), resolved);
        }
        match binding {
            Some(Binding::Def(dm, di)) => self.def_type(dm, di, site, sp),
            Some(Binding::Overloads(om, is)) => Ok(self.overload_type(n, om, &is, site, sp)),
            Some(Binding::Module(_)) => Err(format!(
                "`{n}` is a module; refer to a definition as `{n}.name`"
            )),
            None => Err(format!("unknown name `{n}`")),
        }
    }

    pub(crate) fn def_type(
        &mut self,
        m: usize,
        i: usize,
        site: u32,
        sp: Span,
    ) -> Result<Ty, String> {
        match self.def_scheme(m, i) {
            Some(s) if s.failed => Err(self.failed_use(m, i)),
            Some(s) => self.instantiate(&s, sp, Some(site)),
            None => Ok(self.fresh()),
        }
    }

    pub(crate) fn failed_use(&self, m: usize, i: usize) -> String {
        let name = if m == self.module {
            Some(self.env.defs[i].name.as_str())
        } else {
            self.env.owns.get(&m).and_then(|own| {
                own.iter().find_map(|(n, b)| match b {
                    Binding::Def(dm, di) if (*dm, *di) == (m, i) => Some(n.as_str()),
                    Binding::Overloads(dm, is) if *dm == m && is.contains(&i) => Some(n.as_str()),
                    _ => None,
                })
            })
        };
        match name {
            Some(n) => format!(
                "`{}` has a type error, so this use cannot be checked",
                op_name(n)
            ),
            None => "this name has a type error, so this use cannot be checked".into(),
        }
    }

    // ── coercions at expectations ──────────────────────────────────────────

    /// Unify `actual` with `expected`, allowing constant lifting into `expr`,
    /// int → float and string → date / timestamp widening, expressions as sort keys, and
    /// records as window specs (also inside lists).
    pub(crate) fn coerce(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (a, e) = (self.resolve(actual), self.resolve(expected));
        match (&a, &e) {
            // A record literal is the surface argument of
            // `expr r (row s)` / `agg (expr r (row s))`. Its field
            // expressions are retained in the third slot of `record_expr`;
            // the stage constraint attached to the primitive computes the
            // output row in the fourth slot.
            (Ty::Row(..) | Ty::Empty, Ty::Con("record_expr", ea)) if ea.len() == 4 => {
                self.unify(&ea[2], &a)
            }
            (Ty::Con(s, sa), Ty::Con("expr", ea)) if sa.is_empty() && SCALARS.contains(s) => {
                let v = ea[2].clone();
                self.lift(s, &v)
            }
            (Ty::Con(s, sa), Ty::Con(_, ta))
                if sa.is_empty() && ta.is_empty() && SCALARS.contains(s) =>
            {
                self.lift(s, &e)
            }
            (Ty::Con("expr", ea), Ty::Con("sortkey", ka)) => {
                let (p, r, want) = (ea[0].clone(), ea[1].clone(), ka[0].clone());
                self.key_phase(&p)?;
                self.unify(&r, &want)
            }
            (Ty::Row(..) | Ty::Empty, Ty::Con("winspec", w)) => {
                let r = w[0].clone();
                self.winspec(&a, &r)
            }
            (Ty::Con("list", x), Ty::Con("list", y)) => {
                let (x, y) = (x[0].clone(), y[0].clone());
                self.coerce(&x, &y)
            }
            _ => self.unify(&a, &e),
        }
    }

    pub(crate) fn lift(&mut self, s: &'static str, target: &Ty) -> U {
        match self.resolve(target) {
            Ty::Var(_) if matches!(s, "int" | "string") => {
                self.pending.push((
                    Cons::Lit {
                        lit: s,
                        target: target.clone(),
                    },
                    self.span,
                ));
                Ok(())
            }
            Ty::Con(t, args)
                if args.is_empty()
                    && matches!((s, t), ("int", "float") | ("string", "date" | "timestamp")) =>
            {
                Ok(())
            }
            _ => self.unify(&con(s), target),
        }
    }

    pub(crate) fn key_phase(&mut self, p: &Ty) -> U {
        self.place(Place::Key, p)
    }

    pub(crate) fn winspec(&mut self, spec: &Ty, r: &Ty) -> U {
        let (fs, tail) = self.flatten(spec);
        self.unify(&tail, &Ty::Empty)?;
        for (k, t) in fs {
            let want = match k.as_str() {
                rules::winspec::PARTITION | rules::winspec::ORDER => list(sortkey(r.clone())),
                rules::winspec::FRAME => con("frame"),
                o => {
                    return Err(format!(
                        "unknown window spec field `{o}`; expected {}",
                        rules::winspec::names()
                    ))
                }
            };
            self.coerce(&t, &want)
                .map_err(|m| format!("window spec field `{k}`: {m}"))?;
        }
        Ok(())
    }

    // ── primitives ─────────────────────────────────────────────────────────

    pub(crate) fn prim_type(&mut self, p: Prim, sp: Span) -> Ty {
        use Prim::*;
        let (s, i) = (con("string"), con("int"));
        match p {
            Table => fun(s.clone(), fun(s, query(self.fresh_row()))),
            Where => {
                let (pred, r) = (self.fresh(), self.fresh_row());
                self.pending.push((
                    Cons::Filter {
                        pred: pred.clone(),
                        row: r.clone(),
                    },
                    sp,
                ));
                fun(pred, fun(query(r.clone()), query(r)))
            }
            Select | AggStage => {
                let (f, a, b) = (self.fresh_row(), self.fresh_row(), self.fresh_row());
                let c = Cons::Project {
                    fields: f.clone(),
                    input: a.clone(),
                    output: b.clone(),
                    agg: p == AggStage,
                };
                self.pending.push((c, sp));
                let phase = if p == AggStage {
                    con("agg")
                } else {
                    con("row")
                };
                fun(
                    record_expr(phase, a.clone(), f, b.clone()),
                    fun(query(a), query(b)),
                )
            }
            Update => {
                let (f, a, s, out) = (
                    self.fresh_row(),
                    self.fresh_row(),
                    self.fresh_row(),
                    self.fresh_row(),
                );
                let c = Cons::Update {
                    fields: f.clone(),
                    input: a.clone(),
                    output: s.clone(),
                    result: out.clone(),
                };
                self.pending.push((c, sp));
                fun(
                    record_expr(con("row"), a.clone(), f, s),
                    fun(query(a), query(out)),
                )
            }
            // `omit "k"` drops one column. The rule is the definition's own:
            // the label comes from the signature's `symbol k`, the input row
            // from its `r`, and the output row is `removeKey k r` — the same
            // term the signature names, which is what makes the equation
            // `input ~ { k : t | output }` an obligation of *this* stage rather
            // than something a later pass recovers from a composed result.
            Omit => {
                let (k, r) = (self.fresh(), self.fresh_row());
                let out = Ty::RemoveKey(Box::new(k.clone()), Box::new(r.clone()));
                self.pending.push((
                    Cons::Omit {
                        label: k.clone(),
                        input: r.clone(),
                        output: out.clone(),
                    },
                    sp,
                ));
                fun(Ty::Con("keyLabel", vec![k]), fun(query(r), query(out)))
            }
            // `mapValue "w"` wraps every field type. The rule is the
            // definition's own: the wrapper comes from the signature's
            // `valueLabel w`, the input row from its `r`, and the output row is
            // the term `mapValue w r` the signature names — so the equation is
            // an obligation of *this* stage rather than something a later pass
            // recovers from a composed result.
            MapValue => {
                let (w, r) = (self.fresh(), self.fresh_row());
                let out = Ty::MapValue(Box::new(w.clone()), Box::new(r.clone()));
                self.pending.push((
                    Cons::MapValue {
                        label: w.clone(),
                        output: out.clone(),
                    },
                    sp,
                ));
                fun(Ty::Con("valueLabel", vec![w]), fun(query(r), query(out)))
            }
            Merge => {
                let (left, right, out) = (self.fresh_row(), self.fresh_row(), self.fresh_row());
                self.pending.push((
                    Cons::Merge {
                        left: left.clone(),
                        right: right.clone(),
                        out: out.clone(),
                    },
                    sp,
                ));
                fun(query(left), fun(query(right), query(out)))
            }
            // `Prefix`/`Suffix` are not projections, so their signatures come
            // from the prelude's annotations rather than from here: a
            // `__prefix`/`__suffix` use without one is rejected before this
            // point. They fall to the same catch-all as the other
            // non-projection primitives.
            Order => {
                let (req, r) = (self.fresh(), self.fresh_row());
                self.pending.push((
                    Cons::Within {
                        req: req.clone(),
                        row: r.clone(),
                    },
                    sp,
                ));
                fun(list(sortkey(req)), fun(query(r.clone()), query(r)))
            }
            Limit | Offset => {
                let r = self.fresh_row();
                fun(i, fun(query(r.clone()), query(r)))
            }
            Distinct => {
                let r = self.fresh_row();
                fun(query(r.clone()), query(r))
            }
            In => {
                let (r, a) = (self.fresh_row(), self.fresh());
                fun(
                    list(a.clone()),
                    fun(
                        expr(con("row"), r.clone(), a),
                        expr(con("row"), r, con("bool")),
                    ),
                )
            }
            Join(kind) => {
                let nullable = match kind {
                    JoinKind::Inner => (false, false),
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                    JoinKind::Semi | JoinKind::Anti => (false, false),
                };
                let (l, r, pred, out) = (
                    self.fresh_row(),
                    self.fresh_row(),
                    self.fresh(),
                    self.fresh_row(),
                );
                self.pending.push((
                    Cons::JoinOn {
                        pred: pred.clone(),
                        left: l.clone(),
                        right: r.clone(),
                    },
                    sp,
                ));
                let c = Cons::JoinOut {
                    left: l.clone(),
                    right: r.clone(),
                    out: out.clone(),
                    nullable,
                    left_only: matches!(kind, JoinKind::Semi | JoinKind::Anti),
                };
                self.pending.push((c, sp));
                fun(query(r), fun(pred, fun(query(l), query(out))))
            }
            Set(_) => {
                let (l, r, out) = (self.fresh(), self.fresh(), self.fresh());
                self.pending.push((
                    Cons::Set {
                        left: l.clone(),
                        right: r.clone(),
                        out: out.clone(),
                    },
                    sp,
                ));
                fun(query(l), fun(query(r), query(out)))
            }
            Group => {
                let (r, a) = (self.fresh(), self.fresh());
                fun(
                    expr(con("row"), r.clone(), a.clone()),
                    expr(con("agg"), r, a),
                )
            }
            Asc | Desc => {
                // `asc .x`: a row-phase expression becomes a sort key. Taking
                // an `expr` rather than a `sortkey` rejects `asc (desc .x)`.
                let (r, a) = (self.fresh(), self.fresh());
                fun(expr(con("row"), r.clone(), a), sortkey(r))
            }
            Rows => fun(con("bound"), fun(con("bound"), con("frame"))),
            UnboundedPreceding | UnboundedFollowing | CurrentRow => con("bound"),
            Preceding | Following => fun(i, con("bound")),
            // `prefix "s"` / `suffix "s"` rename every column. The rule is
            // carried by the *type*: the result row is the term
            // `keyMap (prependSym k) r`, over the signature's own label and row,
            // and the row reducer materializes it once the input row is known.
            // There is no separate obligation because the operation is already
            // stated exactly once, in the signature this definition owns.
            Prefix | Suffix => {
                let (k, r) = (self.fresh(), self.fresh_row());
                let label = Ty::Con(
                    if p == Prefix {
                        "labelPrefix"
                    } else {
                        "labelSuffix"
                    },
                    vec![k.clone()],
                );
                fun(
                    Ty::Con("keyLabel", vec![k]),
                    fun(
                        query(r.clone()),
                        query(Ty::MapKey(Box::new(label), Box::new(r))),
                    ),
                )
            }
        }
    }

    // ── deferred constraints ───────────────────────────────────────────────

    /// Solve pending constraints until no more progress; unsolved ones stay
    /// pending (and become part of the enclosing definition's scheme).
    pub(crate) fn solve(&mut self) -> R<()> {
        loop {
            let mut progress = false;
            let mut keep = Vec::new();
            for (c, sp) in std::mem::take(&mut self.pending) {
                match self.step(&c, sp) {
                    Ok(true) => progress = true,
                    Ok(false) => keep.push((c, sp)),
                    Err(msg) => {
                        // Completion probes are checked against a partially
                        // typed expression. Another field in the same join
                        // predicate may still be an unfinished name; keep the
                        // probe row so completion can report the side under
                        // the cursor instead of failing on that sibling.
                        if self.probe.is_some() && msg.contains("no column `") {
                            progress = true;
                            continue;
                        }
                        self.pending = keep;
                        return Err(TyErr { span: sp, msg });
                    }
                }
            }
            // Resolving an overload may have added the candidate's constraints.
            keep.append(&mut self.pending);
            self.pending = keep;
            if !progress {
                return Ok(());
            }
        }
    }

    /// `Ok(true)` when solved, `Ok(false)` when it must wait.
    pub(crate) fn step(&mut self, c: &Cons, sp: Span) -> Result<bool, String> {
        match c {
            Cons::Overload { .. } => self.step_overload(c, sp),
            Cons::Filter { .. } => self.step_filter(c, sp),
            Cons::Project { .. } => self.step_project(c, sp),
            Cons::JoinOn { .. } => self.step_join_on(c, sp),
            Cons::Set { .. } => self.step_set(c, sp),
            Cons::Lit { .. } => self.step_lit(c, sp),
            Cons::Within { .. } => self.step_within(c, sp),
            Cons::Update { .. } => self.step_update(c, sp),
            Cons::Omit { .. } => self.step_omit(c, sp),
            Cons::MapValue { .. } => self.step_map_value(c, sp),
            Cons::Merge { .. } => self.step_merge(c, sp),
            Cons::JoinOut { .. } => self.step_join_out(c, sp),
        }
    }

    fn step_overload(&mut self, c: &Cons, sp: Span) -> Result<bool, String> {
        let Cons::Overload {
            name,
            module,
            cands,
            target,
            origin,
        } = c
        else {
            unreachable!("dispatched on Cons::Overload")
        };
        let fits = self.fitting(*module, cands, target);
        match fits.as_slice() {
            [] => {
                let shown: Vec<String> =
                    cands.iter().map(|&i| self.cand_shown(*module, i)).collect();
                Err(format!(
                    "no overload of `{}` matches {}; candidates: {}",
                    op_name(name),
                    self.show(target),
                    shown.join(", ")
                ))
            }
            [i] => {
                let s = self.cand_scheme(*module, *i);
                if s.failed {
                    return Err(self.failed_use(*module, *i));
                }
                let t = self.instantiate(&s, sp, None)?;
                self.unify(&t, target)?;
                let Origin::Site(site) = *origin else {
                    return Err(format!(
                        "internal error: unresolved hole of `{}`",
                        op_name(name)
                    ));
                };
                let (use_site, k) = decode(site);
                let key = *self.active.last().expect("solving inside a definition");
                self.record(key, use_site, k, Choice::Def(*module, *i));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn step_filter(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Filter { pred, row } = c else {
            unreachable!("dispatched on Cons::Filter")
        };
        match self.resolve(pred) {
            Ty::Var(_) => Ok(false),
            Ty::Con("bool", _) => Ok(true),
            Ty::Con("expr", a) => {
                let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                if is_join(&self.resolve(&r)) {
                    return Err(rules::JOIN_ONLY.into());
                }
                self.place(Place::Where, &p)?;
                if self.row_open(row) {
                    return Ok(false);
                }
                self.unify(row, &r)?;
                self.unify(&v, &con("bool")).map_err(|_| {
                    format!("`where` needs a bool condition, found {}", self.show(&v))
                })?;
                Ok(true)
            }
            o => Err(format!(
                "`where` needs a bool condition, found {}",
                self.show(&o)
            )),
        }
    }

    fn step_project(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Project {
            fields,
            input,
            output,
            agg,
        } = c
        else {
            unreachable!("dispatched on Cons::Project")
        };
        let stage = if *agg { "agg" } else { "select" };
        // A row that is still open cannot yet tell us the value type of a
        // projected label. Keep the projection pending until it closes;
        // consuming it here would leave those values as unconstrained
        // variables.
        if self.row_open(input) {
            return Ok(false);
        }
        let (fs, tail) = self.flatten(fields);
        match tail {
            Ty::Empty => {}
            Ty::Var(_) | Ty::Rigid(..) if fs.is_empty() => return Ok(false),
            _ => {
                let msg = format!(
                    "`{stage}` expects a record of column expressions, found {}",
                    self.show(fields)
                );
                return Err(msg);
            }
        }
        if fs.is_empty() {
            return Err(format!("`{stage}` needs at least one field"));
        }
        if fs
            .iter()
            .any(|(_, t)| matches!(self.resolve(t), Ty::Var(_)))
        {
            return Ok(false);
        }
        let mut out = Vec::new();
        for (l, t) in fs {
            match self.resolve(&t) {
                Ty::Con(s, sa) if sa.is_empty() && SCALARS.contains(&s) => out.push((l, t)),
                Ty::Con("expr", a) => {
                    let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                    if is_join(&self.resolve(&r)) {
                        return Err(format!("field `{l}`: {}", rules::JOIN_ONLY));
                    }
                    self.stage_phase(&p, *agg)
                        .map_err(|m| format!("field `{l}` {m}"))?;
                    self.unify(input, &r)
                        .map_err(|m| format!("field `{l}`: {m}"))?;
                    out.push((l, v));
                }
                o => {
                    let msg = format!("field `{l}` of `{stage}` must be a column expression or constant, found {}", self.show(&o));
                    return Err(msg);
                }
            }
        }
        self.unify(&row(out, Ty::Empty), output)?;
        Ok(true)
    }

    fn step_join_on(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::JoinOn { pred, left, right } = c else {
            unreachable!("dispatched on Cons::JoinOn")
        };
        let (p, r, v) = match self.resolve(pred) {
            Ty::Var(_) => return Ok(false),
            Ty::Con("bool", _) => return Ok(true),
            Ty::Con("expr", a) => (a[0].clone(), a[1].clone(), a[2].clone()),
            o => {
                return Err(format!(
                    "a join predicate must be a bool expression, found {}",
                    self.show(&o)
                ))
            }
        };
        self.place(Place::JoinOn, &p)?;
        match self.resolve(&r) {
            Ty::Con("join", sides) => {
                if [left, right].iter().any(|t| self.row_open(t)) {
                    return Ok(false);
                }
                let (sl, sr) = (sides[0].clone(), sides[1].clone());
                self.unify(left, &sl)
                    .map_err(|m| format!("left join input: {m}"))?;
                self.unify(right, &sr)
                    .map_err(|m| format!("right join input: {m}"))?;
            }
            Ty::Var(_) => self.unify(&r, &Ty::Con("join", vec![left.clone(), right.clone()]))?,
            o => {
                let (fs, _) = self.flatten(&o);
                let n = fs.first().map_or("x", |(k, _)| k.as_str());
                return Err(rules::needs_side(n));
            }
        }
        self.unify(&v, &con("bool"))
            .map_err(|_| format!("a join predicate must be bool, found {}", self.show(&v)))?;
        Ok(true)
    }

    fn step_set(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Set { left, right, out } = c else {
            unreachable!("dispatched on Cons::Set")
        };
        self.unify(left, right)
            .map_err(|m| format!("set-operation inputs: {m}"))?;
        self.unify(out, left)
            .map_err(|m| format!("set-operation output: {m}"))?;
        Ok(true)
    }

    fn step_lit(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Lit { lit, target } = c else {
            unreachable!("dispatched on Cons::Lit")
        };
        match self.resolve(target) {
            Ty::Var(_) => Ok(false),
            _ => self.lift(lit, target).map(|_| true),
        }
    }

    fn step_within(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Within { req, row } = c else {
            unreachable!("dispatched on Cons::Within")
        };
        if self.row_open(row) {
            return Ok(false);
        }
        self.unify(row, req).map(|_| true)
    }

    /// The record of new values must be known, and so must the
    /// input's columns: the output depends on both.
    fn step_update(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Update {
            fields,
            input,
            output,
            result,
        } = c
        else {
            unreachable!("dispatched on Cons::Update")
        };
        let (fs, tail) = self.flatten(fields);
        match tail {
            Ty::Empty => {}
            Ty::Var(_) | Ty::Rigid(..) if fs.is_empty() => return Ok(false),
            _ => {
                return Err(format!(
                    "`update` expects a record of column expressions, found {}",
                    self.show(fields)
                ))
            }
        }
        if fs.is_empty() {
            return Err("`update` needs at least one field".into());
        }
        let mut seen: Vec<String> = Vec::new();
        for (n, _) in &fs {
            if seen.contains(n) {
                return Err(format!("field `{n}` appears twice in `update`"));
            }
            seen.push(n.clone());
        }
        // Each updated expression is checked exactly like a `select`
        // field: row-phase, over the input row. A scalar constant is
        // allowed and lifted the same way.
        let mut values = Vec::new();
        for (l, t) in &fs {
            match self.resolve(t) {
                Ty::Con(s, sa) if sa.is_empty() && SCALARS.contains(&s) => {
                    values.push((l.clone(), None, t.clone()))
                }
                Ty::Con("expr", a) => {
                    let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                    if is_join(&self.resolve(&r)) {
                        return Err(format!("field `{l}`: {}", rules::JOIN_ONLY));
                    }
                    self.stage_phase(&p, false)
                        .map_err(|m| format!("field `{l}` {m}"))?;
                    if self.row_open(input) {
                        return Ok(false);
                    }
                    self.unify(input, &r)
                        .map_err(|m| format!("field `{l}`: {m}"))?;
                    values.push((l.clone(), Some(v.clone()), v));
                }
                o => {
                    return Err(format!(
                        "field `{l}` of `update` must be a column expression or \
                                 constant, found {}",
                        self.show(&o)
                    ))
                }
            }
        }
        let (ifs, itail) = self.flatten(input);
        if self.row_open(input) {
            return Ok(false);
        }
        let named = ifs.iter().map(|(k, _)| (k.clone(), ())).collect::<Vec<_>>();
        let cols = crate::schema::merge_columns(&named, &fs)?;
        // Each output column keeps the input's type, unless the field
        // list replaced it; a name the input does not have is new, so
        // its type comes from the new expression alone.
        let out = cols
            .into_iter()
            .map(|name| {
                let old = ifs.iter().find(|(k, _)| *k == name).map(|(_, t)| t.clone());
                match values.iter().find(|(k, _, _)| *k == name) {
                    Some((_, _, v)) => (name, v.clone()),
                    None => (name, old.unwrap_or_else(|| self.fresh())),
                }
            })
            .collect();
        // `output` is the row of updated fields (`s`), while
        // `result` is the merged query row (`merge r s`).
        let updated = values
            .iter()
            .map(|(name, _, value)| (name.clone(), value.clone()))
            .collect();
        self.unify(&row(updated, Ty::Empty), output)?;
        self.unify(&row_or_tail(out, itail), result)?;
        Ok(true)
    }

    /// `omit "k"` *is* this equation, and the equation is the stage's own:
    /// `output` is `removeKey k input`, so it reduces exactly when the column
    /// is there. The rule belongs to `omit`'s definition and is instantiated
    /// per use, so the label arrives as the signature's `k` bound to the
    /// literal's `Sym` — never recovered from a composed result row.
    ///
    /// The reducer is what tells "not yet" from "not there", which is the
    /// difference between waiting for the row to close and reporting a missing
    /// column. Only this stage reports it, because only this stage owns it.
    fn step_omit(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Omit { label, output, .. } = c else {
            unreachable!("dispatched on Cons::Omit")
        };
        // The label is a `symbol k` until the use site binds it to a literal.
        if !matches!(self.resolve(label), Ty::Sym(_)) {
            return Ok(false);
        }
        match reduce_row(self, output) {
            RowOutcome::Failed { error, .. } => Err(error.message()),
            RowOutcome::Open(..) => Ok(false),
            RowOutcome::Closed(_) => Ok(true),
        }
    }

    /// `mapValue "w"` *is* this equation, and the equation is the stage's own:
    /// `output` is `mapValue w input`, so it reduces exactly when the wrapper
    /// and the row are known. The rule belongs to `mapValue`'s definition and
    /// is instantiated per use, so the wrapper arrives as the signature's `w`
    /// bound to the literal's `Sym` — never recovered from a composed row.
    ///
    /// The reducer is what tells "not yet" from "not a wrapper", which is the
    /// difference between waiting for the row to close and reporting an unknown
    /// wrapper. Only this stage reports it, because only this stage owns it.
    fn step_map_value(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::MapValue { label, output } = c else {
            unreachable!("dispatched on Cons::MapValue")
        };
        // The label is a `valueLabel w` variable until the use site binds it.
        if !matches!(self.resolve(label), Ty::Sym(_)) {
            return Ok(false);
        }
        match reduce_row(self, output) {
            RowOutcome::Failed { error, .. } => Err(error.message()),
            RowOutcome::Open(..) => Ok(false),
            RowOutcome::Closed(_) => Ok(true),
        }
    }

    /// Merge combines two rows; wait if either is open.
    ///
    /// The rule is the reducer's (`reduce_merge`), not a second spelling of it
    /// here: the row this constraint produces and the row a `merge` term
    /// reduces to have to be the same rule, or `update`'s signature and a
    /// `merge` in a signature could disagree about a collision.
    fn step_merge(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::Merge { left, right, out } = c else {
            unreachable!("dispatched on Cons::Merge")
        };
        let (lf, lt) = self.flatten(left);
        let (rf, rt) = self.flatten(right);
        let left = row_or_tail(lf, lt);
        let right = row_or_tail(rf, rt);
        match reduce_merge(&left, &right) {
            Some(merged) => {
                self.unify(&merged, out)?;
                Ok(true)
            }
            // An operand is still open: the merge waits for it.
            None => Ok(false),
        }
    }

    fn step_join_out(&mut self, c: &Cons, _sp: Span) -> Result<bool, String> {
        let Cons::JoinOut {
            left,
            right,
            out,
            nullable,
            left_only,
        } = c
        else {
            unreachable!("dispatched on Cons::JoinOut")
        };
        let (lf, lt) = self.flatten(left);
        let (rf, rt) = self.flatten(right);
        if !matches!(lt, Ty::Empty) || !matches!(rt, Ty::Empty) {
            // Wait for both inputs; with a rigid tail the output
            // columns are not statically known.
            return Ok(false);
        }
        // The far side of an outer join may be missing: its columns
        // become `maybe` (once; `maybe (maybe a)` is `maybe a`). That
        // needs to know which already are: wait for a column whose
        // type may still turn out to be a `maybe`.
        let open = |t: &Ty| match self.resolve(t) {
            Ty::Var(v) => !self.vars[v as usize].nonnull,
            _ => false,
        };
        if (nullable.0 && lf.iter().any(|(_, t)| open(t)))
            || (nullable.1 && rf.iter().any(|(_, t)| open(t)))
        {
            return Ok(false);
        }
        if *left_only {
            self.unify(&row(lf, Ty::Empty), out)?;
            return Ok(true);
        }
        let wrap = |this: &Self, on: bool, (k, t): (String, Ty)| match this.resolve(&t) {
            Ty::Con("maybe", _) => (k, t),
            _ if on => (k, Ty::Con("maybe", vec![t])),
            _ => (k, t),
        };
        let lf: Vec<_> = lf.into_iter().map(|c| wrap(self, nullable.0, c)).collect();
        let rf: Vec<_> = rf.into_iter().map(|c| wrap(self, nullable.1, c)).collect();
        let actual = row(rules::join_columns(&lf, &rf), Ty::Empty);
        // An annotated join wrapper exposes `merge r s` in its result
        // type. Bind those row operands from the actual inputs before
        // comparing the computed output, so fields discovered by the
        // predicate (`.<id`) cannot leave the public merge open.
        let public_merge = matches!(self.resolve(out), Ty::Merge(..));
        if let Ty::Merge(al, ar) = self.resolve(out) {
            let bind_input = |this: &mut Self, input: &Ty, schema: &Ty| {
                match this.resolve(schema) {
                    // Outer-join signatures wrap the missing side in
                    // `mapValue`; the join input itself still has the
                    // unwrapped row.
                    Ty::MapValue(_, row) => this.unify(input, &row),
                    _ => this.unify(input, schema),
                }
            };
            bind_input(self, left, &al)?;
            bind_input(self, right, &ar)?;
        }
        if public_merge {
            // Reduce the public row former after its operands have
            // been tied to the concrete inputs. This keeps later
            // projections from seeing an open `merge` term. The
            // public type is right-biased; the runtime join retains
            // its established left-column collision behavior.
            let Ty::Merge(al, ar) = self.resolve(out) else {
                unreachable!("join output changed while resolving its merge");
            };
            let (alf, alt) = self.flatten(&al);
            let (arf, art) = self.flatten(&ar);
            let al = row_or_tail(alf, alt);
            let ar = row_or_tail(arf, art);
            if let Some(public_row) = reduce_merge(&al, &ar) {
                self.unify(&public_row, out)?;
            }
        } else {
            self.unify(&actual, out)?;
        }
        Ok(true)
    }

    /// Phase rules for one `select` / `agg` field (messages follow the label).
    pub(crate) fn stage_phase(&mut self, p: &Ty, agg: bool) -> U {
        let at = if agg { Place::Agg } else { Place::Select };
        match self.resolve(p) {
            t @ Ty::Con(..) => match phase_of(&t) {
                Some(ph) => rules::place(at, ph),
                None => Err(format!("not a phase: {}", self.show(&t))),
            },
            // Still open, e.g. a helper's parameter (`e => select { x = e }`):
            // a `select` field may become a row or window expression, never
            // an aggregate, so its uses are checked too.
            Ty::Var(v) if !agg => {
                if !self.vars[v as usize].row_or_win {
                    self.trail.push((v, self.vars[v as usize].clone()));
                    self.vars[v as usize].row_or_win = true;
                }
                Ok(())
            }
            Ty::Var(v) if self.vars[v as usize].row_or_win => rules::place(at, Phase::Row),
            _ => self.unify(p, &con("agg")),
        }
    }

    /// Phase rules for a `where` condition, join predicate, or key: row (or
    /// constant). An open phase becomes `row`.
    pub(crate) fn place(&mut self, at: Place, p: &Ty) -> U {
        match phase_of(&self.resolve(p)) {
            Some(ph) => rules::place(at, ph),
            None => self.unify(p, &con("row")),
        }
    }
}
