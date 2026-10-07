//! [`Checker`] inference, annotations, definitions, and constraint solving.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

use crate::rules::{self, Place};

impl<'w> Checker<'w> {
    // ── definitions and schemes ────────────────────────────────────────────

    pub(crate) fn def_scheme(&mut self, m: usize, i: usize) -> Option<Scheme> {
        if let Some(s) = self.schemes.get(&(m, i)) {
            return Some(s.clone());
        }
        if self.active.contains(&(m, i)) || m != self.module {
            // Recursion, or a module that is not
            // loaded before this one (cannot happen for valid imports).
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
        let r = self.check_def(i);
        self.depth -= 1;
        // Use types as far as checking got, even if it failed later on.
        for (id, sp, t, lit) in std::mem::replace(&mut self.uses, saved_uses) {
            let shown = match (lit, self.resolve(&t)) {
                (Some(l), Ty::Var(_)) => l.to_string(),
                _ => self.show(&t),
            };
            self.use_types.insert((m, sp.start, sp.end), shown);
            // The same facts keyed by `ExprId`, which is what a phase building a
            // typed tree from source needs: it walks the AST and asks "what type
            // did *this* node get?". The span map above cannot answer that — it
            // is keyed by location, and the type has already been flattened to a
            // string by then.
            self.use_tys.insert((m, id), self.resolve(&t));
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
                self.generalize(&t, cons)
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
                    // Projection primitives expose `expr r (row s)` (or, for
                    // the aggregate stage, `agg (expr r (row s))`) at the
                    // surface. Their internal type carries the row of field
                    // expressions separately so the constraint solver can
                    // validate and lower the stage.
                    let internal = match &def.body.kind {
                        ExprKind::Primitive(name) => {
                            let p = match name.as_str() {
                                "__where" => Some(Prim::Where),
                                "__select" => Some(Prim::Select),
                                "__agg" => Some(Prim::AggStage),
                                "__update" => Some(Prim::Update),
                                "__innerJoin" => Some(Prim::Join(JoinKind::Inner)),
                                "__leftJoin" => Some(Prim::Join(JoinKind::Left)),
                                "__rightJoin" => Some(Prim::Join(JoinKind::Right)),
                                "__fullJoin" => Some(Prim::Join(JoinKind::Full)),
                                "__semiJoin" => Some(Prim::Join(JoinKind::Semi)),
                                "__antiJoin" => Some(Prim::Join(JoinKind::Anti)),
                                _ => None,
                            };
                            p.map(|p| self.prim_type(p, def.body.span))
                        }
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
                    // `mapValue w r` — wrap every field type of `r` with the
                    // wrapper `w`. The wrapper is read at the application, so a
                    // variable here is not a value wrapper the checker can use.
                    "mapValue" => {
                        let w = self.conv_valuemap(&args[0])?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        Ok(Ty::MapValue(w, Box::new(r)))
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
        // "..."`, `suffix "..."`, `compose ...`), rather than ordinary type
        // constructors. This is needed for concrete affix witnesses such as
        // `prefixAffix (prefix "u_")`.
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

    /// Convert a `ValueMap`-kinded symbol: `(AsNullable)`, `(AsList)`, `(Id)`.
    /// Like a key mapper, the wrapper is closed data read at the application.
    pub(crate) fn conv_valuemap(&mut self, t: &TypeExpr) -> Result<ValueMap, String> {
        let TypeExpr::App { head, args, .. } = t else {
            return Err("expected a value wrapper, such as `(AsNullable)`".into());
        };
        if !args.is_empty() {
            return Err(format!("`{head}` takes no type argument"));
        }
        match head.as_str() {
            "AsNullable" => Ok(ValueMap::AsNullable),
            "AsList" => Ok(ValueMap::AsList),
            "Id" => Ok(ValueMap::Id),
            other => Err(format!(
                "unknown value wrapper `{other}`; expected `Id`, `(AsNullable)`, or `(AsList)`"
            )),
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
            Ty::MapValue(vm, row) => Ty::MapValue(vm, Box::new(self.inst(&row, map, gens))),
            t @ (Ty::Var(_) | Ty::Rigid(..) | Ty::Empty) => t,
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
            failed: false,
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
            Ty::MapValue(vm, row) => Ty::MapValue(vm, Box::new(self.gen(&row, map, gens))),
            t @ (Ty::Gen(_) | Ty::Empty) => t,
        }
    }

    // ── inference ──────────────────────────────────────────────────────────

    pub(crate) fn infer(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let sp = e.span;
        match &e.kind {
            ExprKind::Name(n) => {
                let t = match env.iter().rev().find(|(k, _)| k == n) {
                    Some((_, t)) => t.clone(),
                    None => self.lookup(n, e.id, sp).map_err(at(sp))?,
                };
                self.uses.push((e.id, sp, t.clone(), None));
                Ok(t)
            }
            ExprKind::Lit(l) => {
                let t = con(lit_name(l));
                self.uses.push((e.id, sp, t.clone(), None));
                Ok(t)
            }
            ExprKind::Field(side, n) => {
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
            ExprKind::Proj(base, f) => {
                if let ExprKind::Name(n) = &base.kind {
                    let scope = self.env.scope;
                    if !env.iter().any(|(k, _)| k == n) {
                        if let Some(Binding::Module(t)) = scope.get(n) {
                            let own = self.env.owns[t];
                            let t = match own.get(f).cloned() {
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
            ExprKind::App(f, args) => {
                let mut ft = self.infer(env, f)?;
                // The key arguments of the key stages, read below.
                let mut keys: Vec<(&'static str, String)> = Vec::new();
                for arg in args {
                    // A key argument (`omit "k"`, `prefix "p"`, or `suffix "s"`)
                    // names a column, so its value has to be known when the
                    // query is checked. It is read here, where the syntax still is, and
                    // recorded in the stage's constraint — which keeps column
                    // names out of the type language entirely. A computed key
                    // cannot name a column, so it is refused rather than
                    // deferred. See docs/ROW-TYPES.md.
                    if let Ty::Fun(p, r) = self.resolve(&ft) {
                        // A directional affix parameter is a string that names
                        // the mapper `m`, which the result type uses
                        // (`query (keyMap m r)`). Carrying the mapper is what
                        // lets `prefix p` stay tied to the row it produces —
                        // the argument's type is what binds `m`.
                        let marker = match (&*p, KeyMarker::of(&p)) {
                            (Ty::Con("prefixAffix", args), _) if args.is_empty() => {
                                Some((KeyMarker::Prefix, None))
                            }
                            (Ty::Con("suffixAffix", args), _) if args.is_empty() => {
                                Some((KeyMarker::Suffix, None))
                            }
                            (Ty::Con("prefixAffix", args), _) if args.len() == 1 => {
                                Some((KeyMarker::Prefix, Some(args[0].clone())))
                            }
                            (Ty::Con("suffixAffix", args), _) if args.len() == 1 => {
                                Some((KeyMarker::Suffix, Some(args[0].clone())))
                            }
                            (_, Some(m)) => Some((m, None)),
                            _ => None,
                        };
                        if let Some((marker, carried)) = marker {
                            // A key argument names a column or supplies a label
                            // fragment. When it is a literal, read it here where
                            // the syntax still is and record it in the stage's
                            // witness. When it is *not* a literal the mapping
                            // cannot be applied yet, so the stage is deferred
                            // and the argument's type is what carries the mapper
                            // — which is what types `addPrefix = p => q => q &
                            // prefix p`.
                            let affix = match &arg.kind {
                                ExprKind::Lit(ast::Lit::Str(s)) => Some(s.clone()),
                                _ => None,
                            };
                            self.span = arg.span;
                            match (marker, affix) {
                                // `omit` needs the column name now: it is a row
                                // equation, and there is no deferring away the
                                // fact that the equation names a column.
                                (KeyMarker::Omit, None) => {
                                    return Err(TyErr {
                                        span: arg.span,
                                        msg: marker.literal_msg(),
                                    });
                                }
                                (KeyMarker::Prefix | KeyMarker::Suffix, None) => {
                                    // Deferred affix: the argument's type is a
                                    // string naming the mapper, so the stage
                                    // stays symbolic until that string is
                                    // known.
                                    let at_ = self.infer(env, arg)?;
                                    let m = carried.unwrap_or_else(|| {
                                        Ty::Var(self.fresh_with(false, true, Kind::KeyMap))
                                    });
                                    self.unify(&at_, &directional_string_kind(marker, m.clone()))
                                        .map_err(|msg| TyErr {
                                            span: arg.span,
                                            msg: format!("{}: {msg}", marker.literal_msg()),
                                        })?;
                                    // Remember which mapper this stage's affix
                                    // names, so the constraint built next uses
                                    // it instead of inventing a fresh one.
                                    self.affix_mappers.push((marker, m));
                                    keys.push((marker.tag(), String::new()));
                                    self.deferred_keys.push(marker);
                                }
                                (_, Some(s)) => {
                                    if let Some(m) = carried {
                                        self.affix_mappers.push((marker, m));
                                    }
                                    keys.push((marker.tag(), s));
                                }
                                (_, None) => {
                                    let at_ = self.infer(env, arg)?;
                                    self.coerce(&at_, &con("string")).map_err(|m| TyErr {
                                        span: arg.span,
                                        msg: format!("{}: {m}", marker.literal_msg()),
                                    })?;
                                    keys.push((marker.tag(), String::new()));
                                    self.deferred_keys.push(marker);
                                }
                            }
                            ft = *r;
                            self.key_stage(&keys, &ft)?;
                            self.solve()?;
                            continue;
                        }
                    }
                    let at_ = self.infer(env, arg)?;
                    self.span = arg.span;
                    ft = match self.resolve(&ft) {
                        Ty::Fun(p, r) => {
                            self.coerce(&at_, &p).map_err(at(arg.span))?;
                            if let ExprKind::Lit(l) = &arg.kind {
                                // A lifted literal (`18` in `.age >= 18`) has
                                // the value type it was lifted to.
                                let v = match self.resolve(&p) {
                                    Ty::Con("expr", a) => a[2].clone(),
                                    o => o,
                                };
                                self.uses.push((arg.id, arg.span, v, Some(lit_name(l))));
                            }
                            *r
                        }
                        Ty::Var(_) => {
                            let r = self.fresh();
                            self.unify(&ft, &fun(at_, r.clone()))
                                .map_err(at(arg.span))?;
                            r
                        }
                        o => {
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
                Ok(ft)
            }
            ExprKind::Lambda(p, body) => {
                let pt = self.fresh();
                env.push((p.clone(), pt.clone()));
                let bt = self.infer(env, body);
                env.pop();
                Ok(fun(pt, bt?))
            }
            ExprKind::Record(fs) => {
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
            ExprKind::List(xs) => {
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
            ExprKind::Sql(_) => Err(TyErr {
                span: sp,
                msg: "`sql \"...\"` must be the whole body of a definition with a type signature"
                    .into(),
            }),
            ExprKind::Primitive(_) => Err(TyErr {
                span: sp,
                msg: "`primitive \"...\"` must be the whole body of a definition with a type signature"
                    .into(),
            }),
            ExprKind::Error => Ok(self.fresh()),
        }
    }

    pub(crate) fn lookup(&mut self, n: &str, site: u32, sp: Span) -> Result<Ty, String> {
        match self.env.scope.get(n).cloned() {
            Some(Binding::Def(dm, di)) => self.def_type(dm, di, site, sp),
            Some(Binding::Overloads(om, is)) => Ok(self.overload_type(n, om, &is, site, sp)),
            Some(Binding::Prim(p)) => Ok(self.prim_type(p, sp)),
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

    /// Register a key stage's constraint once its key arguments have been read
    /// and the remaining function is `query input -> query output`. Called
    /// after each key argument, so a partial application still gets its
    /// constraint.
    ///
    /// Two families live here, and they are deliberately different mechanisms:
    ///
    /// * `omit` is a **row equation** (`input ~ { k : t | output }`) solved by
    ///   the unifier's own leftover rule. It constrains the row's *shape*.
    /// * `prefix`/`suffix` build a **`Ty::MapKey` row term**. A rename is not a
    ///   shape constraint — the output label is not a function of the input's
    ///   shape — so it must be carried as a term and reduced when the row is
    ///   known.
    pub(crate) fn key_stage(&mut self, keys: &[(&'static str, String)], ft: &Ty) -> R<()> {
        let Ty::Fun(p, r) = self.resolve(ft) else {
            return Ok(());
        };
        let (Ty::Con("query", inp), Ty::Con("query", out)) = (&*p, &*r) else {
            return Ok(());
        };
        let (input, out_ty) = (inp[0].clone(), out[0].clone());
        let stage_marker = match keys {
            [(t, _)] if *t == KeyMarker::Prefix.tag() => Some(KeyMarker::Prefix),
            [(t, _)] if *t == KeyMarker::Suffix.tag() => Some(KeyMarker::Suffix),
            _ => None,
        };

        let c = match keys {
            [(t, s)] if *t == KeyMarker::Prefix.tag() || *t == KeyMarker::Suffix.tag() => {
                // The signature says `query (keyMap m r)`. Keep that row term
                // intact in the constraint: the solver first ties its mapper
                // and input components to the stage, then reduces it when the
                // input row is known.
                let (sig_key, output) = match self.resolve(&out_ty) {
                    Ty::MapKey(m, _) => (*m, out_ty.clone()),
                    // Not a `keyMap` result: the signature is not one of ours.
                    _ => {
                        let is_prefix = *t == KeyMarker::Prefix.tag();
                        return Err(TyErr {
                            span: self.span,
                            msg: format!(
                                "`{}` must produce a `keyMap` row",
                                if is_prefix { "prefix" } else { "suffix" }
                            ),
                        });
                    }
                };
                let is_prefix = *t == KeyMarker::Prefix.tag();
                // The mapper comes from the result type the signature declared:
                // `query (keyMap m r)`. For a literal affix `m` is concrete
                // (`KeyMapPrefix "u_"`), and for `prefix p` it is the variable
                // the affix parameter's type named — which is what lets the
                // deferred term reduce at a use site, where the argument's type
                // binds that variable.
                let mut key = sig_key.clone();
                // A literal affix names its own witness, read here where the
                // syntax still is. That is what keeps the mapping on the type
                // side: the checker builds the `KeyMap` from the literal instead
                // of letting the literal travel as a type.
                if !s.is_empty() {
                    let km = if is_prefix {
                        KeyMap::Prefix(s.clone())
                    } else {
                        KeyMap::Suffix(s.clone())
                    };
                    key = km.normalize().to_ty();
                }
                // Tie the mappers together. `sig_key` is the mapper named in the
                // *result* type (`query (keyMap m s)`), and the queued one is
                // what the *parameter* named (`affix m`). They are the same
                // variable in a well-formed signature, but a literal affix
                // replaces `key` with a concrete witness — so the result-side
                // mapper must be bound too, or the row term stays symbolic at
                // the very place it is now known.
                let mut mappers = Vec::new();
                if let Some(pos) = self
                    .affix_mappers
                    .iter()
                    .rposition(|(queued, _)| Some(*queued) == stage_marker)
                {
                    let (_, m) = self.affix_mappers.remove(pos);
                    mappers.push(m);
                }
                mappers.push(sig_key);
                for m in mappers {
                    self.unify(&m, &key).map_err(|msg| TyErr {
                        span: self.span,
                        msg: format!("affix mismatch: {msg}"),
                    })?;
                }
                // Register the equation and let the solver discharge it. The
                // input row is usually still an unbound variable here (the
                // stage's argument is checked after it), so reducing now would
                // always defer; the solver runs once the row is known and
                // reduces the term then. `output` remains the complete
                // `keyMap` row term from the signature.
                self.pending.push((
                    Cons::MapKey {
                        marker: stage_marker.expect("key marker checked above"),
                        key,
                        input: self.resolve(&input),
                        output,
                    },
                    self.span,
                ));
                return Ok(());
            }
            // `omit` and `mapValue` constrain the row directly: their result
            // type is `query s`, so `out[0]` *is* the row.
            [(t, k)] if *t == KeyMarker::Omit.tag() => Cons::Omit {
                key: k.clone(),
                input,
                output: out_ty,
            },
            [(t, wrapper)] if *t == KeyMarker::ValueWrapper.tag() => Cons::MapValue {
                wrapper: wrapper.clone(),
                input,
                output: out_ty,
            },
            // The key list is not complete yet (a partial application).
            _ => return Ok(()),
        };
        let sp = self.span;
        self.pending.push((c, sp));
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
            // A key stage reads its key from the application, where the literal
            // still is (`key_stage`). Omit/value mapping use markers; the
            // directional stages also carry their KeyMap witness through the
            // argument and result so those positions cannot drift apart.
            Omit => {
                let (r, out) = (self.fresh_row(), self.fresh_row());
                fun(KeyMarker::Omit.ty(), fun(query(r), query(out)))
            }
            MapValue => {
                let (r, out) = (self.fresh_row(), self.fresh_row());
                fun(KeyMarker::ValueWrapper.ty(), fun(query(r), query(out)))
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
            Prefix => {
                let (r, m) = (
                    self.fresh_row(),
                    Ty::Var(self.fresh_with(false, false, Kind::KeyMap)),
                );
                let out = Ty::MapKey(Box::new(m.clone()), Box::new(r.clone()));
                fun(
                    directional_string_kind(KeyMarker::Prefix, m),
                    fun(query(r), query(out)),
                )
            }
            Suffix => {
                let (r, m) = (
                    self.fresh_row(),
                    Ty::Var(self.fresh_with(false, false, Kind::KeyMap)),
                );
                let out = Ty::MapKey(Box::new(m.clone()), Box::new(r.clone()));
                fun(
                    directional_string_kind(KeyMarker::Suffix, m),
                    fun(query(r), query(out)),
                )
            }
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
            Cons::Overload {
                name,
                module,
                cands,
                target,
                origin,
            } => {
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
            Cons::Filter { pred, row } => match self.resolve(pred) {
                Ty::Var(_) => Ok(false),
                Ty::Con("bool", _) => Ok(true),
                Ty::Con("expr", a) => {
                    let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                    if is_join(&self.resolve(&r)) {
                        return Err(rules::JOIN_ONLY.into());
                    }
                    self.place(Place::Where, &p)?;
                    if matches!(self.resolve(row), Ty::Var(_)) {
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
            },
            Cons::Project {
                fields,
                input,
                output,
                agg,
            } => {
                let stage = if *agg { "agg" } else { "select" };
                // A mapped row with an open inner row cannot yet tell us the
                // value type of a projected mapped label. Keep the projection
                // pending until the `keyMap` term reduces; consuming it here
                // would leave those values as unconstrained variables.
                let (_, input_tail) = self.flatten(input);
                if matches!(
                    input_tail,
                    Ty::Var(_) | Ty::MapKey(..) | Ty::Merge(..) | Ty::MapValue(..)
                ) {
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
            Cons::JoinOn { pred, left, right } => {
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
                        if [left, right]
                            .iter()
                            .any(|t| matches!(self.resolve(t), Ty::Var(_)))
                        {
                            return Ok(false);
                        }
                        let (sl, sr) = (sides[0].clone(), sides[1].clone());
                        self.unify(left, &sl)
                            .map_err(|m| format!("left join input: {m}"))?;
                        self.unify(right, &sr)
                            .map_err(|m| format!("right join input: {m}"))?;
                    }
                    Ty::Var(_) => {
                        self.unify(&r, &Ty::Con("join", vec![left.clone(), right.clone()]))?
                    }
                    o => {
                        let (fs, _) = self.flatten(&o);
                        let n = fs.first().map_or("x", |(k, _)| k.as_str());
                        return Err(rules::needs_side(n));
                    }
                }
                self.unify(&v, &con("bool")).map_err(|_| {
                    format!("a join predicate must be bool, found {}", self.show(&v))
                })?;
                Ok(true)
            }
            Cons::Set { left, right, out } => {
                self.unify(left, right)
                    .map_err(|m| format!("set-operation inputs: {m}"))?;
                self.unify(out, left)
                    .map_err(|m| format!("set-operation output: {m}"))?;
                Ok(true)
            }
            Cons::Lit { lit, target } => match self.resolve(target) {
                Ty::Var(_) => Ok(false),
                _ => self.lift(lit, target).map(|_| true),
            },
            Cons::Within { req, row } => match self.resolve(row) {
                Ty::Var(_) => Ok(false),
                _ => self.unify(row, req).map(|_| true),
            },
            Cons::Update {
                fields,
                input,
                output,
                result,
            } => {
                // The record of new values must be known, and so must the
                // input's columns: the output depends on both.
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
                            if matches!(self.resolve(input), Ty::Var(_)) {
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
                if matches!(
                    itail,
                    Ty::Var(_) | Ty::MapKey(..) | Ty::Merge(..) | Ty::MapValue(..)
                ) {
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
            Cons::Omit { key, input, output } => {
                // `omit` *is* this equation. The unifier's own leftover rule
                // binds `output` to the rest of the row in the input's order,
                // and a missing key is reported through the ordinary
                // `missing()` path. No column computation happens here.
                if matches!(self.resolve(input), Ty::Var(_)) {
                    return Ok(false);
                }
                let t = self.fresh();
                self.unify(input, &row(vec![(key.clone(), t)], output.clone()))?;
                Ok(true)
            }
            Cons::MapKey {
                marker,
                key,
                input,
                output,
            } => {
                // `output ~ keyMap key input`, reduced as soon as both the
                // mapper and the row are known.
                //
                // The invariant this maintains is that **a `MapKey` term's row
                // is the input row**: `keyMap m r` denotes `m` applied to `r`,
                // and `r` itself stays unrenamed. Binding `output` to the
                // *renamed* row here would encode the mapping twice — once in
                // the term and once in the row — so a later stage would apply
                // it again (`prefix "u_" & suffix "_v2"` produced `u_u_id_v2`).
                let mut key = self.resolve(key);
                // A deferred helper carries its affix as an ordinary string,
                // so the mapper witness can be rebound when that helper is
                // instantiated. Preserve the stage direction while doing so;
                // otherwise a stale mapper variable can make `suffix` act as
                // a prefix.
                if let Ty::KeyAffix(_, affix) = &key {
                    let constructor = match marker {
                        KeyMarker::Prefix => "KeyMapPrefix",
                        KeyMarker::Suffix => "KeyMapSuffix",
                        _ => "",
                    };
                    if !constructor.is_empty() {
                        key = Ty::KeyAffix(constructor, affix.clone());
                    }
                }
                let (input_fields, input_tail) = self.flatten(input);
                let input = row_or_tail(input_fields, input_tail);
                let mut output_ty = self.resolve(output);
                if let Ty::MapKey(mapper, row) = output_ty.clone() {
                    if let Ty::KeyAffix(_, affix) = self.resolve(&mapper) {
                        let constructor = match marker {
                            KeyMarker::Prefix => "KeyMapPrefix",
                            KeyMarker::Suffix => "KeyMapSuffix",
                            _ => "",
                        };
                        if !constructor.is_empty() {
                            output_ty = Ty::MapKey(Box::new(Ty::KeyAffix(constructor, affix)), row);
                        }
                    }
                }
                if let Ty::MapKey(mapper, row) = output_ty.clone() {
                    self.unify(&mapper, &key)?;
                    self.unify(&row, &input)?;
                    output_ty = self.zonk(output);
                }
                match reduce_mapkey(&key, &input) {
                    Some(reduced) => {
                        let stage = match KeyMap::of_ty(&key) {
                            Some(KeyMap::Suffix(_)) => "suffix",
                            _ => "prefix",
                        };
                        self.unify(&reduced, &output_ty)
                            .map_err(|m| format!("`{stage}` cannot map its input row: {m}"))?;
                        Ok(true)
                    }
                    // Not reducible *yet*: the mapper is still a helper's
                    // variable, or the row is still open. Stay pending rather
                    // than binding `output` to the symbolic term: once bound, it
                    // could never become the rewritten row, so the reduction
                    // above would never fire. Waiting costs nothing — `solve`
                    // re-runs this after every binding.
                    None => Ok(false),
                }
            }
            Cons::MapValue {
                wrapper,
                input,
                output,
            } => {
                // `mapValue` wraps each field type.
                // If the tail is open (Var or Rigid), defer as a MapValue term.
                let (ifs, itail) = self.flatten(input);

                // Parse the wrapper
                let vm = match wrapper.as_str() {
                    "nullable" => ValueMap::AsNullable,
                    "list" => ValueMap::AsList,
                    "id" => ValueMap::Id,
                    _ => return Err(format!("unknown wrapper `{}`", wrapper)),
                };

                // Handle Id early: no transformation needed
                if matches!(vm, ValueMap::Id) {
                    self.unify(input, output)?;
                    return Ok(true);
                }

                let input_row = row_or_tail(ifs.clone(), itail.clone());

                // Try to reduce at type level first
                match reduce_mapvalue(&vm, &input_row) {
                    Some(reduced) => {
                        // Successful eager reduction - unify with output
                        self.unify(&reduced, output)?;
                        Ok(true)
                    }
                    None => {
                        // Cannot reduce yet (open row)
                        match itail {
                            Ty::Empty => {
                                // Closed row but reduction failed: should not happen
                                // for ValueMap since it has no validation errors
                                unreachable!("reduce_mapvalue should succeed on closed rows")
                            }
                            Ty::Var(_) => {
                                // Open variable tail: wait for it to be bound
                                Ok(false)
                            }
                            Ty::Rigid(..) => {
                                // Open rigid tail: defer as a MapValue term
                                let deferred = Ty::MapValue(vm, Box::new(input_row));
                                self.unify(&deferred, output)?;
                                Ok(true)
                            }
                            _ => unreachable!("flatten only returns Empty, Var, or Rigid tails"),
                        }
                    }
                }
            }
            Cons::Merge { left, right, out } => {
                // Merge combines two rows; wait if either is open
                let (lf, lt) = self.flatten(left);
                let (rf, rt) = self.flatten(right);
                if !matches!(lt, Ty::Empty) || !matches!(rt, Ty::Empty) {
                    return Ok(false);
                }
                // Merge logic: right-wins, keeps left positions, appends right-only
                let mut merged: Vec<(String, Ty)> = lf.clone();
                for (rname, rty) in &rf {
                    if let Some(pos) = merged.iter().position(|(n, _)| n == rname) {
                        // Replace with right's type
                        merged[pos].1 = rty.clone();
                    } else {
                        // Append new field
                        merged.push((rname.clone(), rty.clone()));
                    }
                }
                self.unify(&row(merged, Ty::Empty), out)?;
                Ok(true)
            }
            Cons::JoinOut {
                left,
                right,
                out,
                nullable,
                left_only,
            } => {
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
        }
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
