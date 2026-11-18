//! [`Checker`] inference, annotations, definitions, and constraint solving.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

use crate::rules::{self, Place};

/// Is this a key parameter — the position a stage reads its key from?
///
/// `omit` / `prefix` / `suffix` take a `symbol ?k`, which `conv` carries as a
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
        ExprKind::Name(n) if n == "rename" => {
            "`rename` needs two literal column names, such as `rename \"years\" \"age\"`".into()
        }
        ExprKind::Name(n) if n == "mapValue" => {
            "`mapValue` needs a literal wrapper, such as `mapValue \"nullable\"` or \
             `mapValue \"list\"`"
                .into()
        }
        // A helper's own parameter: name the property, not a stage the helper may
        // have nothing to do with. Suggesting `prefix "user_"` for `rename 1 2`
        // was advice about a different function.
        _ => "this argument must be a literal string naming a column or an affix — the \
              mapping is part of the type, so it must be known when the query is checked"
            .into(),
    }
}

impl<'w> Checker<'w> {
    /// Record advice: the program is well typed, and the reader may want to know
    /// what it does not say.
    ///
    /// Reported against the definition being checked, so a warning travels the
    /// same route to the editor and the command line as an error does, and
    /// kept in its own list so nothing that asks "did this definition check"
    /// has to learn about severity.
    ///
    /// No diagnostic in the language uses this today: the one that did — a join
    /// keeping the left column of a shared name — is gone, because the column is
    /// renamed instead of dropped and there is nothing left to warn about. The
    /// channel stays (the command line and the editor both read it), and these
    /// two are how a future warning reaches it.
    #[allow(dead_code)]
    pub(crate) fn warn(&mut self, span: Span, kind: crate::workspace::DiagKind, message: String) {
        self.warnings.push((span, message, kind, Vec::new()));
    }

    /// A warning that comes with fixes the reader can apply.
    #[allow(dead_code)]
    pub(crate) fn warn_with(
        &mut self,
        span: Span,
        kind: crate::workspace::DiagKind,
        message: String,
        suggestions: Vec<crate::workspace::Suggestion>,
    ) {
        self.warnings.push((span, message, kind, suggestions));
    }

    // ── definitions and schemes ────────────────────────────────────────────

    pub(crate) fn def_scheme(&mut self, m: usize, i: usize) -> Option<Scheme> {
        if m != self.module {
            return self.env.schemes.get(&m)?.get(&(m, i)).cloned();
        }
        if let Some(s) = self.schemes.get(&(m, i)) {
            return Some(s.clone());
        }
        // A name defined *above* this definition is answered by its own
        // memoized check: that is what makes an edit to it invalidate the
        // definitions that read it rather than the whole file.
        //
        // A name defined *below* its use is judged the same way inside the
        // inline chain of `check_def` below — the cycle and depth rules there
        // decide what a forward reference sees, exactly as they did when a
        // module was one query. This branch only serves a caller outside such a
        // chain (an overload candidate asked for a definition it has not
        // reached), which has no cycle of its own to close. Rechecking it
        // inline would make a module of n definitions cost n checks of growing
        // prefixes — which is what checking the whole module in one query
        // already was, and what splitting it apart exists to avoid.
        //
        // A name defined *below* its use is deliberately not routed this way:
        // `check_def` below closes the cycle with `active` and reports it, and a
        // salsa query would make the same program a salsa cycle instead.
        if i < self.current {
            let checked = def_check(self.db, self.input, i);
            let scheme = checked.scheme.clone()?;
            if checked.failed {
                self.failed.insert((m, i));
            }
            self.schemes.insert((m, i), scheme.clone());
            return Some(scheme);
        }
        if self.active.contains(&(m, i)) {
            // A use of a definition that is still being checked is a cycle.
            // Returning a fresh type here used to let `q = q` check as `q : a`
            // and exit successfully with no SQL, because only the normalizer —
            // which runs for query definitions — noticed the recursion.
            // Report it where the cycle closes, then continue with a fresh
            // type so the rest of the definition still gets checked.
            if self.recursion_reported.insert((m, i)) {
                let name = &self.defs.get(i).name;
                self.errors.push(RawTypeError {
                    module: m,
                    def: i,
                    span: self.defs.get(i).span,
                    message: format!("`{name}` refers to itself; recursion is not supported"),
                    diag_kind: crate::workspace::DiagKind::Recursion,
                    kind: RawErrorKind::Def,
                    severity: crate::workspace::Severity::Error,
                    suggestions: Vec::new(),
                });
            }
            let v = self.fresh();
            return Some(self.generalize(&v, vec![]));
        }
        if self.depth >= MAX_DEF_DEPTH {
            // The chain of forward references is deeper than any real program
            // and would overflow the stack before a diagnostic could be
            // reported, so report it once, here, and let the rest of the
            // chain check against a fresh type instead of cascading.
            //
            // Reported at the definition being checked, not at the one the
            // walk happens to stop on: that is the definition whose own type is
            // unknown, and it is the one the reader is looking at. Each check
            // walks its own chain, so a chain of n definitions yields one
            // diagnostic per definition that lies on a chain of its own that is
            // too long — where checking a module in one query reported the
            // single place one shared walk gave up.
            if !self.depth_reported {
                self.depth_reported = true;
                let current = self.defs.get(self.current);
                self.errors.push(RawTypeError {
                    module: m,
                    def: self.current,
                    span: current.span,
                    message: format!(
                        "`{}` lies on a chain of more than {MAX_DEF_DEPTH} forward-referenced \
                         definitions; define a name before the definitions that use it",
                        current.name
                    ),
                    diag_kind: crate::workspace::DiagKind::Recursion,
                    kind: RawErrorKind::Chain,
                    severity: crate::workspace::Severity::Error,
                    suggestions: Vec::new(),
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
        let saved_combines = std::mem::take(&mut self.combines);
        let saved_combine_partials = std::mem::take(&mut self.combine_partials);
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
            // variable chain and stops, so `query ?r` came back with `r` still a
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
        let combines = self.finish_combines();
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
                scheme.combine = self.def_combine(i);
                scheme
            }
            Err(e) => {
                // `unnest .id` reads as a mismatch between two `expr`s whose
                // halves look swapped ("expected `int`, found `list a`"): the
                // column's value variable is bound to the parameter's element
                // before the table's row arrives, so the conflict surfaces in
                // the row rather than at the argument. The operation is what is
                // wrong, so it says so — the recorded argument span is what
                // tells this error apart from any other mismatch.
                let message = if e.msg.contains("type mismatch")
                    && self
                        .unnest_args
                        .iter()
                        // The error is re-spanned at the application, so the
                        // argument's span overlaps rather than contains it.
                        .any(|sp| e.span.start <= sp.end && sp.start <= e.span.end)
                {
                    "`unnest` needs a list, and this is not one; the element under `value` \
                     comes from a list"
                        .to_string()
                } else {
                    e.msg
                };
                self.errors.push(RawTypeError {
                    module: m,
                    def: i,
                    span: e.span,
                    message,
                    diag_kind: e.kind,
                    kind: RawErrorKind::Def,
                    severity: crate::workspace::Severity::Error,
                    suggestions: e.suggestions,
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
            let body = &self.defs.get(i).body;
            let holes = self.holes.get(&(m, i)).copied().unwrap_or(0);
            let recorded = self.choices.get(&(m, i)).cloned().unwrap_or_default();
            let tree = crate::check::typed::build(
                body,
                holes,
                &crate::check::typed::NodeFacts {
                    stages: if succeeded { stages.as_slice() } else { &[] },
                    combines: if succeeded { combines.as_slice() } else { &[] },
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
        self.combines = saved_combines;
        self.combine_partials = saved_combine_partials;
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
        let def = self.defs.get(i);
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
                     from it, e.g. `{} : expr ?r string -> expr ?r string = sql \"UPPER($1)\"`",
                    def.name, def.name
                ))),
            };
        }
        if let ExprKind::Primitive(_) = def.body.kind {
            // A primitive's signature is its type: the type annotation is required
            // and describes the primitive's behavior.
            return match ann {
                Some(t) => {
                    // A **projection** primitive exposes `expr ?r (row ?s)` (or,
                    // for the aggregate stage, `agg (expr ?r (row ?s))`) at the
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
                        // These primitives' *internal* type is their real signature,
                        // and it says something the surface annotation cannot: a join
                        // merges two rows, and a set operation answers with its left
                        // operand's row. `query ?r -> query ?r -> query ?r` is true of a
                        // set operation and loses which `r` the answer is — so the
                        // internal rule wins here rather than being reconciled with the
                        // declaration.
                        if matches!(
                            &def.body.kind,
                            ExprKind::Primitive(name)
                                if matches!(
                                    name.as_str(),
                                    "__innerJoin"
                                        | "__leftJoin"
                                        | "__rightJoin"
                                        | "__fullJoin"
                                        | "__semiJoin"
                                        | "__antiJoin"
                                        | "__union"
                                        | "__unionAll"
                                        | "__intersect"
                                        | "__except"
                                )
                        ) {
                            return Ok(internal);
                        }
                        self.span = def.body.span;
                        self.coerce(&internal, &t)
                            .map_err(|msg| {
                                msg.prefixed(&format!(
                                    "`{}` does not match its signature: ",
                                    def.name
                                ))
                            })
                            .map_err(at(def.body.span))?;
                        return Ok(t);
                    }
                    Ok(t)
                }
                None => Err(at(def.span)(format!(
                    "`{}` needs a type signature: a `primitive` takes its type \
                     from it, e.g. `{} : query ?r -> query ?r' = primitive \"__prefix\"`",
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
                .map_err(|msg| {
                    msg.prefixed(&format!("`{}` does not match its signature: ", def.name))
                })
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
            // A query is normalized, so its overloads must be resolved. Rank
            // them *before* leftover literals are defaulted: `coalesce 0
            // .amount` still has a polymorphic `0`, and defaulting it to `int`
            // can leave no candidate where the nullable signature fits.
            self.arbitrate_pending(false)?;
            self.solve()?;
            self.default_lits();
            self.solve()?;
            self.arbitrate_pending(true)?;
        } else {
            // A non-query definition is not normalized, so its overloads may
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
                let msg = self.no_overload_at(module, &name, &cands, &target, sp);
                return Err(TyErr {
                    span: sp,
                    kind: msg.kind,
                    msg: msg.message,
                    suggestions: Vec::new(),
                });
            }
            // What is still open is a choice the definition leaves to its
            // users. Ranking it now keeps the *signature* honest — `adult =
            // .age >= 18` is `expr { age = a | ?b } bool`, not a result type
            // nobody can name — while the argument types stay as open as they
            // were.
            for (c, sp) in self.pending.clone() {
                if let Cons::Overload {
                    name,
                    module,
                    cands,
                    target,
                    origin,
                } = c
                {
                    let fits = self.fitting(module, &cands, &target);
                    let ranked = self.prefer(module, &fits, &target, false);
                    if ranked.len() == 1 {
                        self.choose(module, ranked[0], &target, &origin, &name, sp)
                            .map_err(|fail| TyErr {
                                span: sp,
                                kind: fail.kind,
                                msg: fail.message,
                                suggestions: Vec::new(),
                            })?;
                    }
                }
            }
        }
        Ok(t)
    }

    /// Choose the overloads that this definition can still resolve, ranking
    /// the survivors by the shape of their arguments.
    ///
    /// With `raise` false a tie is left pending — it may still be broken by a
    /// literal's default or by a later constraint. With `raise` true the
    /// definition cannot wait any more, and a tie is the `ambiguous use` error.
    fn arbitrate_pending(&mut self, raise: bool) -> Result<(), TyErr> {
        for (c, sp) in self.pending.clone() {
            let Cons::Overload {
                name,
                module,
                cands,
                target,
                origin,
            } = c
            else {
                continue;
            };
            let fits = self.fitting(module, &cands, &target);
            if fits.is_empty() {
                // Some literals may still be waiting for their default; the
                // arbitrary-arity message comes from the final pass.
                continue;
            }
            let ranked = self.prefer(module, &fits, &target, false);
            if let [winner] = ranked.as_slice() {
                self.choose(module, *winner, &target, &origin, &name, sp)
                    .map_err(|fail| TyErr {
                        span: sp,
                        kind: fail.kind,
                        msg: fail.message,
                        suggestions: Vec::new(),
                    })?;
                continue;
            }
            if !raise {
                continue;
            }
            let shown: Vec<String> = ranked.iter().map(|&i| self.cand_shown(module, i)).collect();
            let msg = format!(
                "ambiguous use of `{}` at type {}; candidates: {}",
                op_name(&name),
                self.show(&target),
                shown.join(", ")
            );
            return Err(TyErr {
                span: sp,
                kind: crate::workspace::DiagKind::Overload,
                msg,
                suggestions: Vec::new(),
            });
        }
        Ok(())
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
    pub(crate) fn annotation(&mut self, t: &TypeExpr) -> Result<Ty, Fail> {
        let mut res = t;
        while let TypeExpr::Fun(_, r) = res {
            res = r;
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
                _ => return Err(Fail::from(rules::clash(Phase::Agg, Phase::Win))),
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
    ) -> Result<Ty, Fail> {
        match t {
            TypeExpr::Nat(..) => Err(Fail::from(
                "a number is not a type on its own; `decimal p s` is the \
                                      constructor that takes one",
            )),
            TypeExpr::App {
                head, args, var, ..
            } => {
                // A `?a` head is a type variable by construction. It is checked
                // first, because the name after the sigil is arbitrary: `?int`
                // is a variable called `int`, not the constructor.
                if *var {
                    if !args.is_empty() {
                        return Err(Fail::from(format!(
                            "`?{head}` is a type variable and takes no arguments"
                        )));
                    }
                    return Ok(self.rigid(head, names));
                }
                let Some(&(name, arity)) = CONS.iter().find(|(n, _)| *n == head.as_str()) else {
                    // `row ?s` marks a *record of columns* in expression
                    // position, as in `select : expr ?r (row ?s) -> ...`. It is a
                    // one-argument wrapper whose argument is a Row, and it is
                    // erased on conversion — `row ?s` becomes the row `s`, so the
                    // same variable can also appear in `query ?s`. It is handled
                    // here rather than in `CONS` because `row` is also a
                    // zero-argument internal phase marker (`con("row")`), and
                    // registering it as a constructor would shadow that.
                    if head == "row" {
                        if args.len() != 1 {
                            return Err(Fail::from("`row` takes one type argument"));
                        }
                        return self.conv_with_kind(&args[0], phase, names, Kind::Row);
                    }
                    if args.is_empty() {
                        // A bare name is a type *constructor*, and type names
                        // are closed. It is never a variable: variables are
                        // written `?a`. The old rule was "one lowercase letter",
                        // which made `amount = decimal` — and `x = m`, where `m`
                        // was a one-letter typo — a fresh variable per use, so
                        // one column was an `int` in one definition and a
                        // `string` in another. Checked, and wrong.
                        return Err(Fail::kinded(
                            crate::workspace::DiagKind::UnknownName,
                            unknown_type_name(head),
                        ));
                    }
                    return Err(Fail::from(format!("unknown type constructor `{head}`")));
                };
                if args.len() != arity {
                    return Err(Fail::from(format!(
                        "`{name}` takes {arity} type argument(s), got {}",
                        args.len()
                    )));
                }
                match name {
                    "decimal" => {
                        // Each parameter is a number, or a variable that a use
                        // site binds to one — `decimal p s` in a signature is
                        // the same type as `decimal 10 2` written out.
                        let mut params = Vec::new();
                        for arg in &args[..2] {
                            match arg {
                                TypeExpr::Nat(n, _) => params.push(Ty::Nat(*n)),
                                TypeExpr::App {
                                    head,
                                    args,
                                    var: true,
                                    ..
                                } if args.is_empty() => params.push(self.rigid(head, names)),
                                _ => {
                                    return Err(Fail::from(
                                        "`decimal` takes two numbers — `decimal 10 2` \
                                                is ten digits with two after the point; a \
                                                signature may also write `decimal p s` and let \
                                                the use site decide",
                                    ))
                                }
                            }
                        }
                        // A pair of literals is checked where it is written; a
                        // parameter is not, because its value comes from the use.
                        if let (Ty::Nat(p), Ty::Nat(s_)) = (&params[0], &params[1]) {
                            if s_ > p {
                                return Err(Fail::from(format!(
                                    "`decimal {p} {s_}` has more digits after the point than \
                                     it has digits; write `decimal {s_} {s_}` or widen the \
                                     precision"
                                )));
                            }
                            if *p == 0 || *p > 38 {
                                return Err(Fail::from(format!(
                                    "`decimal` precision must be between 1 and 38, and this is \
                                     {p}; 38 is what every engine in the portable family \
                                     supports"
                                )));
                            }
                        }
                        Ok(Ty::Con("decimal", params))
                    }
                    "expr" => {
                        let r = self.conv(&args[0], phase, names)?;
                        // `expr ?r (row ?s)` is the public spelling for a
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
                                    return Err(Fail::from("`row` takes one type argument"));
                                }
                                let output =
                                    self.conv_with_kind(&row_args[0], phase, names, Kind::Row)?;
                                return Ok(record_expr(phase.clone(), r, self.fresh_row(), output));
                            }
                        }
                        let a = self.conv(&args[1], phase, names)?;
                        Ok(expr(phase.clone(), r, a))
                    }
                    // `keyMap ?m ?r` is the row term for applying mapper `m` to
                    // row `r`. Both arguments are kinded: `m` is `KeyMap`, `r`
                    // is `Row`, and the result is a `Row`.
                    "keyMap" => {
                        let m = self.conv_keymap(&args[0], names)?;
                        if kind_of(&m, &self.vars) != Kind::KeyMap {
                            return Err(Fail::from(
                                "kind mismatch: `keyMap` expects a mapper of kind `KeyMap`",
                            ));
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
                    // `merge ?r ?s` — right wins on a name collision, and names
                    // only in `s` are appended. A row former, like `Ty::Merge`.
                    "merge" => {
                        let l = self.conv_with_kind(&args[0], phase, names, Kind::Row)?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        Ok(Ty::Merge(Box::new(l), Box::new(r)))
                    }
                    // `removeKey ?k ?r` — drop the column named by the label `k`
                    // from row `r`. The label is a type (a `symbol ?k` variable
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
                    // `mapValue ?w ?r` — wrap every field type of `r` with the
                    // wrapper `w` names. The witness is a *label*: a closed
                    // `(AsNullable)` names one, and a bare variable is the
                    // `valueLabel ?w` a signature declared, so the operation
                    // travels in the type exactly as `keyMap`'s mapper does.
                    "mapValue" => {
                        let w = self.conv_valuemap_label(&args[0], names)?;
                        let r = self.conv_with_kind(&args[1], phase, names, Kind::Row)?;
                        Ok(Ty::MapValue(Box::new(w), Box::new(r)))
                    }
                    // `keymapper m` introduces a `KeyMap`-kinded variable `m`,
                    // for a signature that takes a mapper as an argument:
                    //   keyMap : keymapper m -> query ?r -> query (keyMap ?m ?r)
                    "keymapper" | "valuemapper" => {
                        let TypeExpr::App { head, args, .. } = &args[0] else {
                            return Err(Fail::from(format!("`{name}` needs a type variable")));
                        };
                        if !args.is_empty() {
                            return Err(Fail::from(format!("`{name}` needs a bare type variable")));
                        }
                        let kind = if name == "keymapper" {
                            Kind::KeyMap
                        } else {
                            Kind::ValueMap
                        };
                        Ok(self.rigid_with_kind(head, names, kind))
                    }
                    // `symbol ?k` — a column label. It is carried as a `keyLabel`
                    // marker around a `Type` variable, so `infer_app` can
                    // recognise a key parameter and read the stage off the
                    // result. The call site binds the variable to the literal's
                    // `Sym`.
                    "symbol" => {
                        let TypeExpr::App { head, args, .. } = &args[0] else {
                            return Err(Fail::from("`symbol` needs a type variable"));
                        };
                        if !args.is_empty() {
                            return Err(Fail::from("`symbol` needs a bare type variable"));
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
                            // expressions, so `agg (expr ?r (row ?s))` must keep
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
                                            return Err(Fail::from(
                                                "`row` takes one type argument",
                                            ));
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
                        _ => Err(Fail::from(format!(
                            "`{name}` wraps an expression type, e.g. `{name} (expr ?r int)`"
                        ))),
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
                for f in fields {
                    let (k, v) = (&f.name, &f.ty);
                    if fs.iter().any(|(o, _)| o == k) {
                        return Err(Fail::from(format!(
                            "field `{k}` appears twice in a record type"
                        )));
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
            // A string in type position is a `symbol`: the label a row former
            // records. `removeKey "a" ?r` is how `omit "a"` states its result, and
            // printing it any other way makes `--types` unwritable — the reader is
            // told the type of their definition in a notation no file can contain.
            //
            // A key mapper's affix is the same thing and is handled by
            // `conv_keymap`, which needs the literal for a different reason: the
            // mapping is part of the type, so it must be known at check time.
            TypeExpr::Str(s, _) => Ok(Ty::Sym(s.clone())),
            TypeExpr::Error(_) => Ok(self.fresh()),
        }
    }

    pub(crate) fn conv_with_kind(
        &mut self,
        t: &TypeExpr,
        phase: &Ty,
        names: &mut HashMap<String, Ty>,
        expected_kind: Kind,
    ) -> Result<Ty, Fail> {
        // Mapper arguments use their own surface grammar (`id`, `prefix
        // "..."`, `suffix "..."`, `prependSym k`, `compose ...`), rather than
        // ordinary type constructors, so they are converted by their own
        // routine.
        if expected_kind == Kind::KeyMap {
            return self.conv_keymap(t, names);
        }
        match t {
            TypeExpr::Nat(..) => Err(Fail::from(
                "a number is not a type; `decimal p s` is the type that takes one",
            )),
            TypeExpr::App { head, args, .. } => {
                // `row ?s` is the record-in-expression-position marker; its
                // argument is a Row regardless of the kind expected here. See
                // the same case in `conv`.
                if head == "row" {
                    if args.len() != 1 {
                        return Err(Fail::from("`row` takes one type argument"));
                    }
                    return self.conv_with_kind(&args[0], phase, names, Kind::Row);
                }
                let Some(&(_name, _arity)) = CONS.iter().find(|(n, _)| *n == head.as_str()) else {
                    if args.is_empty() {
                        return Ok(self.rigid_with_kind(head, names, expected_kind));
                    }
                    return Err(Fail::from(format!("unknown type constructor `{head}`")));
                };
                // If it's a known constructor, use regular conv
                self.conv(t, phase, names)
            }
            TypeExpr::Record { fields, tail, .. } => {
                // Records are always rows, regardless of expected_kind
                let mut fs: Vec<(String, Ty)> = Vec::new();
                for f in fields {
                    let (k, v) = (&f.name, &f.ty);
                    if fs.iter().any(|(o, _)| o == k) {
                        return Err(Fail::from(format!(
                            "field `{k}` appears twice in a record type"
                        )));
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
    /// bare type variable is the `valueLabel ?w` a signature declared — the same
    /// variable in the parameter and in the result row, which is what lets the
    /// operation travel in the type instead of being read at each application.
    pub(crate) fn conv_valuemap_label(
        &mut self,
        t: &TypeExpr,
        names: &mut HashMap<String, Ty>,
    ) -> Result<Ty, Fail> {
        match t {
            TypeExpr::App { head, args, .. } if args.is_empty() => match head.as_str() {
                "AsNullable" => Ok(Ty::Sym("nullable".to_string())),
                "AsList" => Ok(Ty::Sym("list".to_string())),
                "Id" => Ok(Ty::Sym("id".to_string())),
                _ => Ok(self.rigid(head, names)),
            },
            _ => Err(Fail::from(
                "expected a value wrapper, such as `(AsNullable)`, or a `valueLabel` variable",
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
    /// polymorphic in the mapping: `keyMap ?m ?r` with `m` a variable is how a
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
    ) -> Result<Ty, Fail> {
        let TypeExpr::App { head, args, .. } = t else {
            return Err(Fail::from(
                "expected a key mapper, such as `(prefix \"u_\")`",
            ));
        };
        match head.as_str() {
            "id" => {
                if !args.is_empty() {
                    return Err(Fail::from("`id` takes no type argument"));
                }
                Ok(KeyMap::Id.to_ty())
            }
            // `renameSym ?to ?from` — the rename mapper. Both labels are
            // `symbol` variables, so it stays open until the call site binds
            // them, exactly as `prependSym` does.
            "renameSym" => {
                let [to, from] = args.as_slice() else {
                    return Err(Fail::from("`renameSym` takes two symbol arguments"));
                };
                let mut bare = |t: &TypeExpr| -> Result<Ty, Fail> {
                    let TypeExpr::App { head, args, .. } = t else {
                        return Err(Fail::from("`renameSym` needs bare symbol variables"));
                    };
                    if !args.is_empty() {
                        return Err(Fail::from("`renameSym` needs bare symbol variables"));
                    }
                    Ok(self.rigid(head, names))
                };
                // `(from, to)`, the order `KeyMap::Rename` and `to_ty` use. The
                // two labels were briefly stored `(to, from)` here while the
                // decoder read `(from, to)`, which applied the rename backwards:
                // `rename "years" "age"` looked for a column called `years`, found
                // none, and left the row alone — with the right SQL, because the
                // normalizer reads the names from the value rather than the type.
                let (f, t) = (bare(from)?, bare(to)?);
                Ok(Ty::Con("labelRename", vec![f, t]))
            }
            "prefix" | "suffix" => {
                let [arg] = args.as_slice() else {
                    return Err(Fail::from(format!(
                        "`{head}` takes one type argument, the affix"
                    )));
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
            // label is a `symbol ?k` variable. It stays open until the call site
            // binds `k` to a literal, then `resolve_mapper` turns it into the
            // concrete `KeyAffix`. Unlike `prefix "x"`, no literal is needed.
            "prependSym" | "appendSym" => {
                let [label] = args.as_slice() else {
                    return Err(Fail::from(format!("`{head}` takes one symbol argument")));
                };
                let TypeExpr::App {
                    head: lh, args: la, ..
                } = label
                else {
                    return Err(Fail::from(format!("`{head}` needs a symbol variable")));
                };
                if !la.is_empty() {
                    return Err(Fail::from(format!("`{head}` needs a bare symbol variable")));
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
                    return Err(Fail::from("`compose` takes two key mappers"));
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
                    return Err(Fail::from(format!(
                        "unknown key mapper `{other}`; expected `id`, `(prefix \"s\")`, \
                         `(suffix \"s\")`, `(compose f g)`, or a mapper variable"
                    )));
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
                        return Err(Fail::from(format!(
                            "kind mismatch: `{other}` is not a key mapper"
                        )));
                    }
                    // A bare variable of kind KeyMap, bound by `keymapper` in a
                    // signature or inferred at a use. This is the deferred case:
                    // it carries no transformation yet.
                    return Ok(self.rigid_with_kind(other, names, Kind::KeyMap));
                }
                Err(Fail::from(format!(
                    "unknown key mapper `{other}`; expected `id`, `(prefix \"s\")`, \
                     `(suffix \"s\")`, `(compose f g)`, or a mapper variable"
                )))
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
        // A parameter's annotation names a type the argument has, not one the
        // definition promises to work at universally (see `Checker::par_ann`).
        let t = if self.par_ann {
            Ty::Var(self.fresh_with(false, true, kind))
        } else {
            Ty::Rigid(self.fresh_with(false, true, kind), name.to_string())
        };
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
    ) -> Result<Ty, Fail> {
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
            Ty::Nat(n) => Ty::Nat(n),
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
            combine: None,
            failed: false,
        }
    }

    /// The stage a definition is, from its body. A `primitive` whose signature
    /// is a query stage owns that stage's row rule.
    pub(crate) fn def_stage(&self, i: usize) -> Option<RowStage> {
        match &self.defs.get(i).body.kind {
            ExprKind::Primitive(name) => {
                crate::primitive::prim_of(name).and_then(RowStage::of_prim)
            }
            _ => None,
        }
    }

    pub(crate) fn def_combine(&self, i: usize) -> Option<CombineKind> {
        match &self.defs.get(i).body.kind {
            ExprKind::Primitive(name) => match crate::primitive::prim_of(name) {
                Some(crate::primitive::Prim::Join(kind)) => Some(CombineKind::Join(kind)),
                Some(crate::primitive::Prim::Set(kind)) => Some(CombineKind::Set(kind)),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn gen(&self, t: &Ty, map: &mut HashMap<u32, u32>, gens: &mut Vec<GenInfo>) -> Ty {
        match self.resolve(t) {
            Ty::Nat(n) => Ty::Nat(n),
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
            // `(e : ty)`: the expression is checked *against* the type rather
            // than inferred and then unified, which is what makes the ascription
            // an escape hatch — a literal widens, and an open row is closed.
            ExprKind::Ascribe { expr, ty } => {
                let want = self.annotation(ty).map_err(at(e.span))?;
                let inner = self.infer(env, expr)?;
                self.pending.push((
                    Cons::Ascribe {
                        inner,
                        want: want.clone(),
                    },
                    e.span,
                ));
                Ok(want)
            }
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
            // position: `expr ?r (row ?s)`, `agg (expr ?r (row ?s))`, a `winspec`.
            // Its fields are named, so the expected row supplies each field's
            // type rather than each field being unified against a fresh
            // variable.
            // `(e : ty)`: `e` is checked against the *ambient* expectation, so
            // its row and phase come from where it was written — a field
            // inferred with no expectation would invent a row of its own — and
            // the annotation is a second, deferred obligation on the value.
            ExprKind::Ascribe { expr, ty } => {
                let want = self.annotation(ty).map_err(at(e.span))?;
                let inner = self.check(env, expr, expected)?;
                self.pending.push((Cons::Ascribe { inner, want }, e.span));
                Ok(expected.clone())
            }
            ExprKind::Record(..) => self.check_record(env, e, expected),
            // A lambda's parameter type comes from the expected function type
            // when it has one.
            ExprKind::Lambda(..) => self.check_lambda(env, e, expected),
            // A list literal's element type comes from the expected `list t`.
            ExprKind::List(..) => self.check_list(env, e, expected),
            // A string literal checked against a `symbol ?k` label parameter
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
                    suggestions: Vec::new(),
                    span: e.span,
                    kind: crate::workspace::DiagKind::Other,
                    msg: format!("field `{name}` appears twice"),
                });
            }
            // The field's expectation comes from the expected row when it has
            // one. A *stage*'s record is checked against
            // `record_expr(phase, row, fields, output)`, whose field types are
            // not named yet, so a field the expectation does not mention gets a
            // fresh variable; the projection's own constraint unifies those
            // with the input row once the row is known.
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
    /// An annotation on the parameter (`(x : string) => ...`) wins over the
    /// expectation and is compared with it, because the reader wrote it to pin
    /// the type down: agreeing silently would let `(x : int) => x` pass where a
    /// `string -> string` is wanted, and would report the mismatch at the
    /// argument instead of at the annotation that caused it.
    ///
    /// The body is inferred exactly once and its type *is* the result type of
    /// the function type returned; inferring the whole lambda as well would
    /// visit every subexpression twice.
    fn check_lambda(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr, expected: &Ty) -> R<Ty> {
        let ExprKind::Lambda(param, body) = &e.kind else {
            unreachable!("dispatched on ExprKind::Lambda")
        };
        let annotated = self.param_ty(param).map_err(at(param.span))?;
        let want = self.resolve(expected);
        // Peel one arrow when the expectation has one. When it is not a
        // function there is nothing to draw the parameter type from, so
        // synthesis is the only option — or the annotation, when there is one.
        let from_expected = match &want {
            Ty::Fun(p, _) => Some((**p).clone()),
            _ => None,
        };
        let param_ty = match (annotated, from_expected) {
            (Some(ann), Some(expect)) => {
                self.coerce(&ann, &expect).map_err(at(param.span))?;
                ann
            }
            (Some(ann), None) => ann,
            (None, Some(expect)) => expect,
            (None, None) => {
                let actual = self.infer(env, e)?;
                self.coerce(&actual, expected).map_err(at(e.span))?;
                return Ok(self.resolve(expected));
            }
        };
        env.push((param.name.clone(), param_ty.clone()));
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
        if n != PROBE_FIELD {
            self.field_uses.push((n.clone(), sp));
        }
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
                                kind: crate::workspace::DiagKind::Other,
                                msg: format!("module `{n}` has no definition `{f}`"),
                                suggestions: Vec::new(),
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

    /// `toDecimal 10 2 x` — the two numbers are the parameters of the result
    /// type, so the checker reads them where they are written.
    ///
    /// This is not a coercion: the parameters of a `decimal` are part of its
    /// type, and they are literals because the SQL cast needs the width in the
    /// statement's syntax. A non-literal is refused by the backend (which needs
    /// `Expr::Number`), and here it simply leaves the parameters open, which the
    /// next use reports.
    /// The value of an integer literal argument, looking through a definition
    /// that is one.
    ///
    /// `toDecimal 10 2 x` is the usual spelling; `p = 10` followed by
    /// `toDecimal p 2 x` is the same program, and the normalizer inlines the
    /// constant for the SQL, so the type has to see it too rather than leaving
    /// the parameters open.
    fn int_literal(&mut self, e: &ast::Expr) -> Option<u32> {
        match &e.kind {
            ExprKind::Lit(ast::Lit::Int(n)) if *n >= 0 => Some(*n as u32),
            ExprKind::Name(_) => {
                let resolved = self.resolutions.get(&(self.module, e.id))?.clone();
                let Resolved::Def((m, i)) = resolved else {
                    return None;
                };
                if m != self.module {
                    return None;
                }
                let body = &self.defs.get(i).body;
                match &body.kind {
                    ExprKind::Lit(ast::Lit::Int(n)) if *n >= 0 => Some(*n as u32),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn decimal_params(&mut self, f: &ast::Expr, args: &[ast::Expr], result: &Ty) {
        let ExprKind::Name(name) = &f.kind else {
            return;
        };
        let expect = match name.as_str() {
            "toDecimal" | "tryDecimal" => 2,
            _ => return,
        };
        if args.len() < expect {
            return;
        }
        let mut nums = Vec::new();
        for arg in &args[..expect] {
            match self.int_literal(arg) {
                Some(n) => nums.push(n),
                None => return,
            }
        }
        // The result is `expr ?r (decimal p s)` — or `expr ?r (maybe (decimal p s))`
        // for `tryDecimal`, and a *function* when the cast is partially applied
        // (`lift (toDecimal 10 2) .d`): the parameters are inside the wrappers,
        // and behind however many arrows one argument left.
        let mut t = self.resolve(result);
        loop {
            t = self.resolve(&t);
            match &t {
                Ty::Con("expr", a) if a.len() == 3 => t = a[2].clone(),
                Ty::Con("maybe", a) if a.len() == 1 => t = a[0].clone(),
                Ty::Fun(_, r) => t = (**r).clone(),
                _ => break,
            }
        }
        if let Ty::Con("decimal", slots) = &t {
            for (slot, n) in slots.iter().zip(nums) {
                let _ = self.unify(slot, &Ty::Nat(n));
            }
        }
    }

    fn infer_app(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::App(f, args) = &e.kind else {
            unreachable!("dispatched on ExprKind::App")
        };
        let mut ft = self.infer(env, f)?;
        // `sum -1` inside a field: `-` applied to a *function*, which is the same slip
        // `& limit -1` is one level up. Checked here because at this point the callee is
        // still known to be `-`/`+` and its first argument is the reader's node — inside
        // the loop the failure has already been raised against a parameter type.
        if let Some(msg) = self.negative_operand(f, args) {
            return Err(at(e.span)(msg));
        }
        // `over spec (rowNumber)`: a window function wrapped in `over`, which wants an
        // aggregate. Reported here because the argument's own type is still available;
        // by the time the application's unification fails, the two `over` overloads have
        // been tried and the message only says `expected agg (…), found winspec ?a ->
        // win (…)` — true, but silent about the function never needing `over`.
        if let Some(msg) = self.over_applied_to_a_window(f, args) {
            return Err(at(e.span)(msg));
        }
        let combine_kind = self.callee_combine_kind(f);
        let mut combine_inputs = Vec::new();
        // `unnest .x` is recorded before the parameter type is pushed down: the
        // failure that needs the name is the one where the value slot is bound
        // too early, and that can happen before this block decides it is a
        // stage application.
        let callee_stage = self.callee_row_stage(f);
        for (index, arg) in args.iter().enumerate() {
            if callee_stage == Some(RowStage::Unnest) {
                self.unnest_args.push(arg.span);
            }
            // Where a join's parts are written, for the warning that offers a
            // rename. The signature's order is `query right -> pred -> query
            // left`, so the index names the part.
            if matches!(combine_kind, Some(CombineKind::Join(_))) {
                let slot = self
                    .join_args
                    .entry((f.span.start, f.span.end))
                    .or_default();
                match index {
                    0 => slot.right = Some(arg.span),
                    1 => slot.pred = Some(arg.span),
                    2 => slot.left = Some(arg.span),
                    _ => {}
                }
            }
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
                // the composed row. See docs/CORE.md.
                // The stage this callee *is*, when its definition owns a row
                // rule. A helper's scheme says nothing, so its use is not a
                // stage application: the stages it applies were recorded in its
                // own body, against its own parameter rows.
                let stage = self.callee_row_stage(f);
                if is_key_parameter(&p) && !is_key_argument(arg) {
                    return Err(TyErr {
                        suggestions: Vec::new(),
                        span: arg.span,
                        kind: crate::workspace::DiagKind::Other,
                        msg: key_argument_message(f),
                    });
                }
                // Where each field's *value* is written, keyed by the stage's
                // own span — which is the span its constraint is posted at.
                // A projection's types are decided when the constraint runs,
                // long after this record literal is gone, so a fix that has to
                // rewrite one field needs its span kept here.
                if matches!(
                    stage,
                    Some(RowStage::Select | RowStage::Agg | RowStage::Update)
                ) {
                    if let ExprKind::Record(fields) = &arg.kind {
                        self.field_spans.insert(
                            (f.span.start, f.span.end),
                            fields.iter().map(|(n, e)| (n.clone(), e.span)).collect(),
                        );
                    }
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
            let combine_param;
            ft = match self.resolve(&ft) {
                Ty::Fun(p, r) => {
                    let p = *p;
                    combine_param = Some(p.clone());
                    // `check` re-records a literal argument at the type it was
                    // lifted to (`18` in `.age >= 18` has the value type it was
                    // lifted to), so no special case is needed here.
                    // `& limit -1` reads as `& (limit - 1)`: `-` binds tighter than
                    // `&`, so the reader wrote a subtraction from a *function*.
                    //
                    // Reported **before** the argument is checked, not after.
                    // Checking it raises `expected expr ?a b, found expr ?a int ->
                    // query ?a -> query ?a` from a queued constraint inside `solve` — a
                    // failure one level below the `&` whose parameter is what the
                    // reader actually wrote — so rewriting that error here never sees
                    // it.
                    if let Some(msg) = self.negative_argument(arg) {
                        return Err(at(arg.span)(msg));
                    }
                    let checked = self.check(env, arg, &p)?;
                    // A `sortkey` result is what says this argument is a *key*:
                    // `asc` and `desc` are the only producers, and they take an
                    // `expr`, which an aggregate and a window also are. Their
                    // phase is the thing that disqualifies them, so it is
                    // checked here, where the result type still describes the
                    // position — the alternative, checking in `coerce`, cannot
                    // tell a key from any other `expr` expectation.
                    if matches!(self.resolve(&r), Ty::Con("sortkey", _)) {
                        if let Some(phase) = self.value_phase(&checked) {
                            self.key_phase(&phase).map_err(at(arg.span))?;
                        }
                    }
                    *r
                }
                Ty::Var(_) => {
                    let at_ = self.infer(env, arg)?;
                    combine_param = Some(at_.clone());
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
                    //
                    // Unless the argument is the mistake. `true and false` is a
                    // reader writing the word for `&&`, and "cannot apply a value
                    // of type bool" is about the `true` they got right — it never
                    // mentions the name that is not a name. When the argument is a
                    // bare identifier this language does not have, that is the
                    // message.
                    if let ast::ExprKind::Name(unknown) = &arg.kind {
                        let known = self.env.scope.contains_key(unknown);
                        if !known {
                            if let Some(what) = crate::suggest::from_another_language(unknown) {
                                return Err(TyErr {
                                    suggestions: Vec::new(),
                                    span: arg.span,
                                    kind: crate::workspace::DiagKind::Other,
                                    msg: format!("`{unknown}` is not this language's name: {what}"),
                                });
                            }
                        }
                    }
                    let msg = format!(
                        "cannot apply a value of type {} to an argument",
                        self.show(&o)
                    );
                    return Err(TyErr {
                        suggestions: Vec::new(),
                        span: arg.span,
                        kind: crate::workspace::DiagKind::Other,
                        msg,
                    });
                }
            };
            if (combine_kind.is_some() || !self.combine_partials.is_empty())
                && combine_param
                    .as_ref()
                    .is_some_and(|p| matches!(self.resolve(p), Ty::Con("query", _)))
            {
                combine_inputs.push(self.zonk(combine_param.as_ref().expect("checked above")));
            }
            self.solve()?;
        }
        // Record the **application's own** type after solving, not before.
        //
        // `ft` is only partly determined mid-loop: for `.age + 1` the type is
        // `expr ?r a` with `a` still an unsolved variable, and it becomes `int`
        // only once `solve` has run the constraints the earlier arguments
        // queued. Recording the pre-solve form put a variable in the map, which
        // a reader sees as `Unknown` — a type that looks recorded and is not
        // usable, which is worse than recording nothing.
        self.solve()?;
        let actual_query_inputs: Vec<Ty> = args
            .iter()
            .filter_map(|arg| self.uses.iter().rev().find(|(id, _, _, _)| *id == arg.id))
            .map(|(_, _, ty, _)| self.zonk(ty))
            .filter(|ty| matches!(self.resolve(ty), Ty::Con("query", _)))
            .collect();
        if let Some(kind) = combine_kind {
            let expected = match kind {
                CombineKind::Join(_) => 3,

                CombineKind::Set(_) => 2,
            };
            if args.len() == expected && combine_inputs.len() == 2 {
                let mut combine_inputs = if actual_query_inputs.len() == 2 {
                    actual_query_inputs
                } else {
                    combine_inputs
                };
                if let Ty::Con("query", _) = self.resolve(&ft) {
                    if matches!(kind, CombineKind::Join(_)) {
                        combine_inputs.reverse();
                    }
                    self.record_combine(kind, e.id, f.span, combine_inputs, self.zonk(&ft));
                }
            } else if args.len() == expected - 1
                && combine_inputs.len() == 1
                && !matches!(f.kind, ast::ExprKind::App(..))
            {
                // The pipe operator applies the final query argument in a
                // separate application. Keep the first query boundary on the
                // partially applied primitive until that outer application is
                // checked.
                let first = actual_query_inputs
                    .into_iter()
                    .next()
                    .or_else(|| combine_inputs.pop())
                    .expect("combine query argument");
                self.combine_partials.insert(e.id, (kind, first));
            } else if args.len() == 1 {
                // A piped application whose callee is the partially applied
                // primitive completes the operation.
                if let ast::ExprKind::App(partial, _) = &f.kind {
                    if let Some((partial_kind, first)) = self.combine_partials.remove(&partial.id) {
                        if partial_kind == kind && combine_inputs.len() == 1 {
                            let mut inputs = vec![combine_inputs.remove(0), first];
                            if matches!(kind, CombineKind::Set(_)) {
                                inputs.swap(0, 1);
                            }
                            if let Ty::Con("query", _) = self.resolve(&ft) {
                                self.record_combine(kind, e.id, f.span, inputs, self.zonk(&ft));
                            }
                        }
                    }
                }
            }
        } else {
            // The pipeline combinator applies a partially applied join or set
            // primitive as its second argument. The primitive itself is not
            // the callee of this application, so recover its recorded first
            // boundary from that argument's identity.
            for arg in args {
                if let Some((kind, first)) = self.combine_partials.remove(&arg.id) {
                    let left = actual_query_inputs
                        .first()
                        .cloned()
                        .or_else(|| combine_inputs.first().cloned())
                        .or_else(|| {
                            self.uses.iter().rev().find_map(|(id, _, ty, _)| {
                                args.iter().any(|a| a.id == *id).then(|| self.zonk(ty))
                            })
                        });
                    if let Some(left) = left {
                        let inputs = vec![left, first];
                        if let Ty::Con("query", _) = self.resolve(&ft) {
                            self.record_combine(kind, arg.id, arg.span, inputs, self.zonk(&ft));
                        }
                    }
                    break;
                }
            }
        }
        self.decimal_params(f, args, &ft);
        self.uses.push((e.id, e.span, self.resolve(&ft), None));
        Ok(ft)
    }

    fn infer_lambda(&mut self, env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::Lambda(p, body) = &e.kind else {
            unreachable!("dispatched on ExprKind::Lambda")
        };
        // Nothing constrains a bare parameter, so it starts as a variable the
        // body settles. An annotated one is already fixed, which is the whole
        // point of writing it: the body is checked against the type the reader
        // meant, not against whatever its first use happens to imply.
        let pt = match self.param_ty(p).map_err(at(p.span))? {
            Some(t) => t,
            None => self.fresh(),
        };
        env.push((p.name.clone(), pt.clone()));
        let bt = self.infer(env, body);
        env.pop();
        Ok(fun(pt, bt?))
    }

    /// The type a lambda's parameter is annotated with, if it has one.
    ///
    /// The conversion is a signature's, so `(q : query { id = int }) => ...`
    /// means here exactly what the same text means after a definition's `:`.
    /// Its type variables differ: they are unknowns the argument settles rather
    /// than universals the lambda must work for (see `Checker::par_ann`), since
    /// a parameter is monomorphic and its annotation describes the one argument
    /// it received.
    fn param_ty(&mut self, p: &ast::LambdaParam) -> Result<Option<Ty>, Fail> {
        let Some(t) = p.ty.as_deref() else {
            return Ok(None);
        };
        let outer = std::mem::replace(&mut self.par_ann, true);
        let ty = self.annotation(t);
        self.par_ann = outer;
        ty.map(Some)
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
                    suggestions: Vec::new(),
                    span: sp,
                    kind: crate::workspace::DiagKind::Other,
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
        // A list of column expressions keeps the form its first element has:
        // `[.a]` is a list of expressions and `[asc .a]` is a list of sort
        // keys. It used to be "a list of keys" whenever the elements were
        // column expressions, which is what made a bare key mean ascending —
        // in *every* position, including a partition, where direction is not a
        // thing. Now the position decides: `order` wants keys and reports a
        // missing `asc`, `partition` wants expressions and takes either.
        let elem = match self.resolve(first) {
            Ty::Con("expr" | "sortkey", _) => self.resolve(first),
            _ => first.clone(),
        };
        for (t, s) in &ts {
            // A direction marker inside a list whose first element was bare is
            // ignored: `partition = [.a, desc .b]` is a partition either way,
            // because a partition has no direction. Only a list *position*
            // does this — `asc (desc .x)` stays an error, which is why the rule
            // is here and not in `coerce`.
            if let (Ty::Con("expr", none), Ty::Con("sortkey", ka)) =
                (self.resolve(&elem), self.resolve(t))
            {
                self.unify(&ka[0], &none[1]).map_err(at(*s))?;
                self.key_phase(&none[0]).map_err(at(*s))?;
                continue;
            }
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
            kind: crate::workspace::DiagKind::Other,
            msg: "`sql \"...\"` must be the whole body of a definition with a type signature"
                .into(),
            suggestions: Vec::new(),
        })
    }

    /// `case { cond => value; ... }`.
    ///
    /// One shared result type and one row, exactly like `ifThenElse`, whose
    /// type is `expr ?r bool -> expr ?r a -> expr ?r a -> expr ?r a`: every
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
                kind: crate::workspace::DiagKind::Other,
                msg: "`case` needs at least one arm: write `cond => value`, and end with \
                      `true => ...` for a default"
                    .into(),
                suggestions: Vec::new(),
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
                    kind: crate::workspace::DiagKind::Other,
                    msg: "a `case` must end with a default arm, written `true => value`, so \
                          that it always produces a value"
                        .into(),
                    suggestions: Vec::new(),
                })
            }
        }
        // The phase is a plain unconstrained variable, *not*
        // `fresh_col_phase()`: a `case` is phase-polymorphic like
        // `ifThenElse`, whose signature declares `expr ?r a -> expr ?r a` with
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
            kind: crate::workspace::DiagKind::Other,
            msg: "`primitive \"...\"` must be the whole body of a definition with a type signature"
                .into(),
            suggestions: Vec::new(),
        })
    }

    fn infer_error(&mut self, _env: &mut Vec<(String, Ty)>, e: &ast::Expr) -> R<Ty> {
        let ExprKind::Error = &e.kind else {
            unreachable!("dispatched on ExprKind::Error")
        };
        Ok(self.fresh())
    }

    pub(crate) fn lookup(&mut self, n: &str, site: u32, sp: Span) -> Result<Ty, Fail> {
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
            Some(Binding::Module(_)) => Err(Fail::kinded(
                crate::workspace::DiagKind::UnknownName,
                format!("`{n}` is a module; refer to a definition as `{n}.name`"),
            )),
            None => {
                // A name from another language first: it is the common case for a
                // reader arriving from SQL, and the edit-distance hint actively
                // misleads there — `null` is six edits from `nothing`, and three
                // from `fullJoin`, so the near-miss would name something unrelated.
                if let Some(what) = crate::suggest::from_another_language(n) {
                    return Err(Fail::kinded(
                        crate::workspace::DiagKind::UnknownName,
                        format!("`{n}` is not this language's name: {what}"),
                    ));
                }
                let hint =
                    crate::suggest::did_you_mean(n, self.env.scope.keys().map(String::as_str));
                Err(Fail::kinded(
                    crate::workspace::DiagKind::UnknownName,
                    format!("unknown name `{n}`{hint}"),
                ))
            }
        }
    }

    pub(crate) fn def_type(&mut self, m: usize, i: usize, site: u32, sp: Span) -> Result<Ty, Fail> {
        match self.def_scheme(m, i) {
            Some(s) if s.failed => Err(self.failed_use(m, i)),
            Some(s) => self.instantiate(&s, sp, Some(site)),
            None => Ok(self.fresh()),
        }
    }

    pub(crate) fn failed_use(&self, m: usize, i: usize) -> Fail {
        let name = if m == self.module {
            Some(self.defs.get(i).name.as_str())
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
            Some(n) => Fail::from(format!(
                "`{}` has a type error, so this use cannot be checked",
                op_name(n)
            )),
            None => Fail::from("this name has a type error, so this use cannot be checked"),
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
            // `expr ?r (row ?s)` / `agg (expr ?r (row ?s))`. Its field
            // expressions are retained in the third slot of `record_expr`;
            // the stage constraint attached to the primitive computes the
            // output row in the fourth slot.
            (Ty::Row(..) | Ty::Empty, Ty::Con("record_expr", ea)) if ea.len() == 4 => {
                self.unify(&ea[2], &a)
            }
            // The other direction: a signature declares `expr ?r (row ?s)` and the
            // argument is a record literal. Its fourth slot is the row the
            // record produces, which is what `s` names — for a `select` that is
            // also computed by the stage constraint, and for a join's `take` it
            // is the only place the row is named at all.
            (Ty::Con("record_expr", ra), Ty::Con("expr", ea)) if ra.len() == 4 => {
                let (p, r) = (ra[0].clone(), ra[1].clone());
                let (out, want) = (ra[3].clone(), ea[2].clone());
                self.unify(&p, &ea[0])?;
                self.unify(&r, &ea[1])?;
                self.unify(&out, &want)
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
            // A sort key must say its direction. This used to accept a bare
            // expression and read it as ascending, which made `order [.a]` and
            // `order [asc .a]` two spellings of one thing — and left the reader
            // guessing whether a missing `asc` was deliberate or forgotten.
            // The coercion still runs, so the message can name the fix.
            // A value that *is* a value type with no expression wrapper — a
            // list literal (`[1, 2]`), which the checker types directly — reads
            // as an expression over no row in an expression position, exactly
            // like the row-free expression above reads as a value in the other
            // direction. Only `list` is here: every other value type is a
            // scalar, and a scalar lifts through `lift` (which is about
            // literals, not about rows).
            (Ty::Con("list", _), Ty::Con("expr", ea)) if ea.len() == 3 => self.unify(&ea[2], &a),
            // A row expression that does not read a row is a value: `year
            // currentDate` has the type `expr ?r int` with `r` unconstrained,
            // because nothing in it refers to a column. Reading it where a
            // scalar is expected is what makes the value position work without
            // a second overload per function — and it is *why* `.d` keeps its
            // expression type: a column reference binds its row, so the
            // coercion does not apply and the overload is chosen by the
            // column's declared type instead of being ambiguous.
            // `expr` carries three slots where the phase is explicit and two
            // where it is the row phase, so both shapes are read here.
            (Ty::Con("expr", ea), want)
                if (ea.len() == 3 || ea.len() == 2)
                    && self.is_row_free(ea)
                    && matches!(
                        self.resolve(want),
                        Ty::Con(s, sa) if sa.is_empty() && SCALARS.contains(&s)
                    ) =>
            {
                let value = if ea.len() == 3 { &ea[2] } else { &ea[1] };
                self.unify(value, want)
            }
            // A join predicate: `bool` in the signature, `maybe bool` accepted.
            //
            // The row `(join ?r ?s)` appears in no other position, so this is
            // exactly the join predicate and nothing else. SQL's own join
            // semantics already say `NULL = anything` is never true, which is
            // what joining on a key that an outer join made nullable wants — so
            // the value needs no rewrite, only the type system's permission.
            //
            // Without it a three-table left join whose second key had gone
            // nullable could not be written at all, and the only advice the
            // compiler gave (`?? 0`) emits `COALESCE(key, 0)`, which matches a
            // NULL-extended row to a real row whose key is 0.
            (Ty::Con("expr", a), Ty::Con("expr", e))
                if a.len() == 3
                    && e.len() == 3
                    && matches!(self.resolve(&e[1]), Ty::Con("join", _))
                    && matches!(self.resolve(&a[2]), Ty::Con("maybe", m) if matches!(self.resolve(&m[0]), Ty::Con("bool", b) if b.is_empty())) =>
            {
                self.unify(&a[0], &e[0])?;
                self.unify(&a[1], &e[1])
            }
            (Ty::Con("expr", ea), Ty::Con("sortkey", _)) => {
                self.key_phase(&ea[0].clone())?;
                Err(Fail::from(
                    "a sort key must say its direction: write `asc <expr>` or `desc <expr>` — \
                     `order [asc .a, desc .b]`"
                        .to_string(),
                ))
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

    /// Does this expression read a row? `expr ?p r a` does when `r` is bound to
    /// one. A free row means the expression is built from constants, which is
    /// what makes `year currentDate` a value.
    ///
    /// The phase matters too: an aggregate (`agg (expr row ?r a)`) has a free
    /// inner row in the same way, and it is *not* a value — it is a row
    /// computation, which the phase slot says.
    fn is_row_free(&self, ea: &[Ty]) -> bool {
        let (row, phase_ok) = if ea.len() == 3 {
            (
                &ea[1],
                matches!(
                    self.resolve(&ea[0]),
                    Ty::Con("row", _) | Ty::Con("const", _) | Ty::Var(_)
                ),
            )
        } else {
            (&ea[0], true)
        };
        phase_ok && matches!(self.resolve(row), Ty::Var(_))
    }

    pub(crate) fn key_phase(&mut self, p: &Ty) -> U {
        self.place(Place::Key, p)
    }

    /// The phase a value's type carries, in the form the placement rules take.
    ///
    /// An `expr` names its phase in its first slot; an aggregate or a window is
    /// its own phase. A constant has none, which is why `None` here is fine:
    /// constants are allowed at every place.
    fn value_phase(&self, t: &Ty) -> Option<Ty> {
        match self.resolve(t) {
            Ty::Con("expr", ea) => Some(ea[0].clone()),
            Ty::Con("agg", _) => Some(con("agg")),
            Ty::Con("win", _) => Some(con("win")),
            _ => None,
        }
    }

    pub(crate) fn winspec(&mut self, spec: &Ty, r: &Ty) -> U {
        let (fs, tail) = self.flatten(spec);
        self.unify(&tail, &Ty::Empty)?;
        for (k, t) in fs {
            let want = match k.as_str() {
                // A partition key is a row expression, *not* a sort key: the two
                // differ in key phase, and `sortkey` here made a partition
                // element that was still a hole become a sort key — so
                // `partition = [year .at]` asked `year` for a `sortkey`.
                rules::winspec::PARTITION => {
                    let a = self.fresh();
                    let want = list(expr(con("row"), r.clone(), a.clone()));
                    // A partition key ignores a direction marker: `[asc .x]` is
                    // `[.x]` here. Accepting it *only* in this position keeps
                    // `asc (desc .x)` an error.
                    if let (Ty::Con("list", xs), Ty::Con("list", _)) = (self.resolve(&t), &want) {
                        for x in xs {
                            let e = expr(con("row"), r.clone(), a.clone());
                            match self.resolve(&x) {
                                Ty::Con("sortkey", ka) => self.unify(&ka[0], r)?,
                                other => self.coerce(&other, &e)?,
                            }
                        }
                        continue;
                    }
                    want
                }
                rules::winspec::ORDER => list(sortkey(r.clone())),
                rules::winspec::FRAME => con("frame"),
                o => {
                    return Err(Fail::from(format!(
                        "unknown window spec field `{o}`; expected {}",
                        rules::winspec::names()
                    )))
                }
            };
            self.coerce(&t, &want)
                .map_err(|m| format!("window spec field `{k}`: {m}"))?;
        }
        Ok(())
    }

    /// The nullable advice for a failed overload, if one applies.
    ///
    /// The `maybe` is *inside* a row expression (`expr ?r (maybe int)`), so the
    /// comparison is between the candidates' value slots. The sentence names the
    /// *argument's* type rather than a candidate's parameter: the parameter is
    /// written with the signature's variables (`decimal a b`), and what the reader
    /// has in hand is the column.
    fn overload_nullable_note(
        &mut self,
        module: usize,
        cands: &[usize],
        target: &Ty,
    ) -> Option<String> {
        let value = |this: &Self, t: &Ty| -> Ty {
            match this.resolve(t) {
                Ty::Con("expr", ea) if ea.len() >= 2 => this.resolve(&ea[ea.len() - 1]),
                other => other,
            }
        };
        for &i in cands {
            let sig = self.cand_scheme(module, i);
            let (trail, nvars) = (self.trail.len(), self.vars.len());
            let t = self.inst(&sig.ty, &mut HashMap::new(), &sig.gens);
            let mut c = t;
            let mut u = target.clone();
            let mut note = None;
            while let (Ty::Fun(cp, cr), Ty::Fun(up, ur)) = (self.resolve(&c), self.resolve(&u)) {
                let (cv, av) = (value(self, &cp), value(self, &up));
                if let Some(n) = crate::check::overload::nullable_note(&cv, &av, self) {
                    note = Some(n);
                    break;
                }
                c = (*cr).clone();
                u = (*ur).clone();
            }
            while self.trail.len() > trail {
                let (v, old) = self.trail.pop().expect("trail entry");
                if (v as usize) < nvars {
                    self.vars[v as usize] = old;
                }
            }
            self.vars.truncate(nvars);
            if note.is_some() {
                return note;
            }
        }
        None
    }

    /// `no overload` for a use at `sp`, labelled with the field it is in.
    ///
    /// An overload failure is reported when the constraint is solved, which is long
    /// after the projection field holding it was checked — so the message named the
    /// operator and not the field. `select { x = .name + 1 }` and
    /// `select { x = upper .id }` are the same mistake in the same place and read
    /// differently: one says which field, the other does not. The field spans are
    /// already kept (for the nullable rewrite); this asks them which field holds
    /// this use.
    /// Is `over` being given a function that is already a window?
    ///
    /// `over : winspec ?r -> agg (expr ?r a) -> win (expr ?r a)` turns an *aggregate* into a
    /// window. `rowNumber` and its relatives are already `winspec ?r -> win (…)`, so
    /// `over { … } (rowNumber)` cannot type-check — and the reader who wrote it most
    /// likely wanted `rowNumber { … }`, which is the same spec without the wrapper.
    ///
    /// The test is on the argument's own type, which is why this runs before the
    /// application is unified: the two `over` overloads are tried in order, and the
    /// failure that surfaces is between `agg (… c)` and `agg (… (maybe c))` — a detail
    /// of the overloads rather than of what the reader wrote.
    fn over_applied_to_a_window(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::Expr],
    ) -> Option<String> {
        let ast::ExprKind::Name(name) = &callee.kind else {
            return None;
        };
        if name != "over" {
            return None;
        }
        let ast::ExprKind::Name(window) = &args.last()?.kind else {
            return None;
        };
        let binding = self.env.scope.get(window).cloned()?;
        let ty = match binding {
            Binding::Def(m, i) => self.def_scheme(m, i)?.ty,
            Binding::Overloads(m, is) => self.def_scheme(m, *is.first()?)?.ty,
            Binding::Module(_) => return None,
        };
        let Ty::Fun(first, rest) = self.resolve(&ty) else {
            return None;
        };
        // The result is `expr ?r (win …)` — `win` is the *phase* argument of `expr`, not a
        // type of its own — so the second half is checked through it. Measured: the
        // resolved shape is `Con("winspec", [Gen(0)]) -> Con("expr", [Con("win", []), …])`.
        let phase = match self.resolve(&rest) {
            Ty::Con("expr", mut ea) if ea.len() == 3 => ea.remove(0),
            _ => return None,
        };
        if !matches!(self.resolve(&first), Ty::Con("winspec", _))
            || !matches!(self.resolve(&phase), Ty::Con("win", _))
        {
            return None;
        }
        Some(
            "`over` takes an aggregate to turn into a window, and this is already a \
             window function: it takes the spec itself. Write `rowNumber { order = \
             [asc .x] }` — and keep `over { … } (sum .n)` for aggregates"
                .to_string(),
        )
    }

    /// `sum -1` in a field: a negative argument that `-` took as a subtraction from a
    /// *function*.
    ///
    /// The same precedence slip as `& limit -1`, one level down. `& limit -1` is caught
    /// where the stage is an argument of `&`; `agg { x = sum -1 }` is caught here, because
    /// the application is `_-_ sum 1` and the reader's mistake is the *first operand*.
    /// Both messages say the same thing: the operator binds tighter than the reader
    /// expected, and parentheses fix it.
    ///
    /// Narrow on purpose — the operand must be a name whose **type** is a function or an
    /// `agg`/`win` producer, so `age - 1` on a column is left alone.
    fn negative_operand(&mut self, callee: &ast::Expr, args: &[ast::Expr]) -> Option<String> {
        let ast::ExprKind::Name(op) = &callee.kind else {
            return None;
        };
        let spelling = op.strip_prefix('_')?.strip_suffix('_')?;
        if !matches!(spelling, "-" | "+") || args.len() != 2 {
            return None;
        }
        let ast::ExprKind::Name(operand) = &args[0].kind else {
            return None;
        };
        let shown = match &args[1].kind {
            ast::ExprKind::Lit(cagara_syntax::ast::Lit::Int(v)) => v.to_string(),
            ast::ExprKind::Lit(cagara_syntax::ast::Lit::Float(v)) => v.to_string(),
            _ => return None,
        };
        let binding = self.env.scope.get(operand).cloned()?;
        let ty = match binding {
            Binding::Def(m, i) => self.def_scheme(m, i)?.ty,
            Binding::Overloads(m, is) => self.def_scheme(m, *is.first()?)?.ty,
            Binding::Module(_) => return None,
        };
        let resolved = self.resolve(&ty);
        let not_a_number = match &resolved {
            Ty::Fun(..) => true,
            other => {
                let rendered = self.show(other);
                let result = rendered.rsplit(" -> ").next().unwrap_or("");
                result.starts_with("agg") || result.starts_with("win")
            }
        };
        if !not_a_number {
            return None;
        }
        Some(format!(
            "`{operand} {spelling}{shown}` reads as `{operand} {spelling} {shown}`: `{spelling}` \
             binds tighter than the argument list, so this subtracts from `{operand}` rather \
             than passing it a negative argument. Write `{operand} ({spelling}{shown})`"
        ))
    }

    /// `& stage -1`: a negative argument that `-` took as a subtraction.
    ///
    /// `-` is infix at a tighter precedence than `&`, so `limit -1` parses as
    /// `limit - 1` — subtracting one from the `limit` *function* — and the diagnostic
    /// that follows talks about function types the reader never wrote. The spelling
    /// that works is `limit (-1)`, and that is what this says.
    ///
    /// Narrow on purpose: the argument must be a numeric operator applied to a name
    /// that **resolves to a function**. That is the condition that makes this not a
    /// guess — a function is never a number, so `stage - 1` cannot be a program, while
    /// `n - 1` on an ordinary value is left alone. That is why the name's *type* is
    /// looked up rather than its spelling.
    fn negative_argument(&mut self, arg: &ast::Expr) -> Option<String> {
        let ast::ExprKind::App(f, args) = &arg.kind else {
            return None;
        };
        let ast::ExprKind::Name(op) = &f.kind else {
            return None;
        };
        let spelling = op.strip_prefix('_')?.strip_suffix('_')?;
        if !matches!(spelling, "-" | "+") || args.len() != 2 {
            return None;
        }
        let ast::ExprKind::Name(callee) = &args[0].kind else {
            return None;
        };
        // The operand as written, when it is a literal: the reader recognises their own
        // `-1` faster than a description of it.
        let shown = match &args[1].kind {
            ast::ExprKind::Lit(cagara_syntax::ast::Lit::Int(v)) => v.to_string(),
            ast::ExprKind::Lit(cagara_syntax::ast::Lit::Float(v)) => v.to_string(),
            _ => return None,
        };
        // Is the left operand something a number cannot be? A stage is a function;
        // `sum` is an `expr ?r a -> agg (expr ?r a)`, and `sum - 1` is the same slip in
        // a field. A plain number or column is neither, and is left alone — which is
        // why the name's *type* is looked up rather than its spelling.
        let binding = self.env.scope.get(callee).cloned()?;
        let ty = match binding {
            Binding::Def(m, i) => self.def_scheme(m, i)?.ty,
            Binding::Overloads(m, is) => self.def_scheme(m, *is.first()?)?.ty,
            Binding::Module(_) => return None,
        };
        let resolved = self.resolve(&ty);
        let not_a_number = match &resolved {
            // A stage: `limit`, `where`, `order` — and `sum`, whose result is an
            // `agg (expr ?r a)`.
            Ty::Fun(..) => true,
            other => {
                let shown = self.show(other);
                let result = shown.rsplit(" -> ").next().unwrap_or("");
                result.starts_with("agg") || result.starts_with("win")
            }
        };
        if !not_a_number {
            return None;
        }
        Some(format!(
            "`{callee} {spelling}{shown}` reads as `{callee} {spelling} {shown}`: `{spelling}` \
             binds tighter than `&`, so this subtracts from the `{callee}` stage instead of \
             passing it a negative argument. Write `{callee} ({spelling}{shown})`"
        ))
    }

    fn no_overload_at(
        &mut self,
        module: usize,
        name: &str,
        cands: &[usize],
        target: &Ty,
        sp: Span,
    ) -> Fail {
        let msg = self.no_overload_message(module, name, cands, target);
        let message = match self.field_label(sp) {
            Some(l) => format!("field `{l}`: {msg}"),
            None => msg,
        };
        Fail::kinded(crate::workspace::DiagKind::Overload, message)
    }

    /// The projection field whose expression contains `sp`, if any.
    ///
    /// `field_spans` is keyed by the record literal's span and was added for the
    /// nullable rewrite, which needs a field's own span to edit. This asks the same
    /// map the other question — which field is this use in — and the answer is the
    /// innermost containing one, because a nested record is a field of a field.
    fn field_label(&self, sp: Span) -> Option<String> {
        self.field_spans
            .values()
            .flatten()
            .filter(|(_, f)| f.start <= sp.start && sp.end <= f.end)
            .min_by_key(|(_, f)| f.end - f.start)
            .map(|(n, _)| n.clone())
    }

    /// The `no overload` diagnostic, with each candidate's first disagreement.
    ///
    /// Listing the signatures alone makes the reader compare three of them
    /// against one use by hand, and the use is the part that is usually wrong.
    /// Saying *where* each candidate parts company turns that into one line per
    /// candidate: `argument 1 expects int, the use has string`.
    fn no_overload_message(
        &mut self,
        module: usize,
        name: &str,
        cands: &[usize],
        target: &Ty,
    ) -> String {
        let mut msg = format!(
            "no overload of `{}` matches {}",
            op_name(name),
            self.show(target)
        );
        // Division is the one arithmetic operator money does not have a form of,
        // and the reason is the engines rather than the type system: PostgreSQL
        // keeps a decimal quotient exact and DuckDB promotes it to `DOUBLE`, so a
        // same-type rule would be a promise one target breaks. Saying that here is
        // more useful than a list of candidates that were never going to match.
        // A `maybe` where a present value is wanted: the mismatch path offers the
        // `??` fix, and the overload path should not be silent about it.
        if self.show(target).contains("maybe") {
            if let Some(note) = self.overload_nullable_note(module, cands, target) {
                msg.push_str(&note);
            }
        }
        if name == "_/_" && self.show(target).contains("decimal") {
            msg.push_str(
                "\n  `decimal` division has no overload: PostgreSQL keeps a quotient exact and \
                 DuckDB promotes it to `DOUBLE`, so no same-type rule holds on both; divide \
                 with `toFloat`, then `toDecimal p s` the result",
            );
        }
        for &i in cands {
            let sig = self.cand_shown(module, i);
            match self.divergence(module, i, target) {
                Some(why) => msg.push_str(&format!("\n  `{sig}`: {why}")),
                // Nothing concrete to compare: the candidate is not *ruled out*
                // by a type, so say that rather than inventing a difference.
                None => msg.push_str(&format!("\n  `{sig}`: differs elsewhere")),
            }
        }
        msg
    }

    // ── primitives ─────────────────────────────────────────────────────────

    pub(crate) fn prim_type(&mut self, p: Prim, sp: Span) -> Ty {
        use Prim::*;
        let (s, i) = (con("string"), con("int"));
        match p {
            Table => fun(s.clone(), fun(s, query(self.fresh_row()))),
            Unnest => {
                // `expr ?r (list a) -> query ?r -> query (merge ?r { value = a })`:
                // the element joins the input row under `value`.
                let (r, a) = (self.fresh_row(), self.fresh());
                let xs = expr(con("row"), r.clone(), list(a.clone()));
                let extra = row(vec![("value".to_string(), a)], Ty::Empty);
                fun(
                    xs,
                    fun(
                        query(r.clone()),
                        query(Ty::Merge(Box::new(r), Box::new(extra))),
                    ),
                )
            }
            Lift => {
                // `(expr ?r a -> expr ?r b) -> expr ?r (maybe a) -> expr ?r (maybe b)`
                let (a, b, r) = (self.fresh(), self.fresh(), self.fresh_row());
                let x = expr(con("row"), r.clone(), Ty::Con("maybe", vec![a.clone()]));
                let f = fun(
                    expr(con("row"), r.clone(), a),
                    expr(con("row"), r.clone(), b.clone()),
                );
                let out = expr(con("row"), r, Ty::Con("maybe", vec![b]));
                fun(f, fun(x, out))
            }
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
            // the label comes from the signature's `symbol ?k`, the input row
            // from its `r`, and the output row is `removeKey ?k ?r` — the same
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
            // `valueLabel ?w`, the input row from its `r`, and the output row is
            // the term `mapValue ?w ?r` the signature names — so the equation is
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
            // `markExists q on`: the same predicate as `semiJoin`, but the answer
            // is a *value* rather than a filter. The row it reads is the ambient
            // one — that is what `Cons::JoinOn` says, exactly as for a join — so
            // the result is a plain bool expression and composes with `&&`, `||`
            // and a projection field.
            MarkExists => {
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
                // The output row is `merge ?l { exists = bool }`; `Cons::JoinOut` is
                // the rule that computes a merge, reused rather than re-derived so
                // the two cannot disagree.
                let exists = self.fresh_row();
                let _ = self.unify(
                    &exists,
                    &Ty::Row(
                        vec![("exists".to_string(), con("bool"))],
                        Box::new(Ty::Empty),
                    ),
                );
                self.pending.push((
                    Cons::JoinOut {
                        left: l.clone(),
                        right: exists,
                        out: out.clone(),
                        nullable: (false, false),
                        left_only: false,
                    },
                    sp,
                ));
                fun(query(r), fun(pred, fun(query(l), query(out))))
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
            Filtered | DistinctAgg | Over => {
                // `filtered p a`, `distinctAgg a`, `over spec a`: the aggregate
                // keeps its own result type, so the signature says what it
                // says and the arena rewrites the SQL. `agg (expr ?r a)` is the
                // type of an aggregate whose argument is one row expression —
                // every aggregate here is one.
                let (r, a) = (self.fresh_row(), self.fresh());
                let agg = Ty::Con("agg", vec![expr(con("row"), r.clone(), a.clone())]);
                match p {
                    Filtered => fun(expr(con("row"), r, con("bool")), fun(agg.clone(), agg)),
                    DistinctAgg => fun(agg.clone(), agg),
                    Over => {
                        let w = self.fresh();
                        fun(
                            Ty::Con("winspec", vec![w.clone()]),
                            fun(agg, Ty::Con("win", vec![expr(con("win"), w, a)])),
                        )
                    }
                    _ => unreachable!("dispatched on the three aggregate modifiers"),
                }
            }
            Set(_) => {
                // `left` and `right` are the operands in **source** order, which is
                // what the arena and the emitted SQL also mean by left and right.
                // `&` is reverse application, so the piped operand — the one written
                // first — is the *last* parameter: `a & union b` is `union b a`.
                //
                // Naming these by parameter order is how a set operation's output row
                // came to be bound to the wrong operand, so that `a & union b` had
                // `b`'s column order in its type and `a`'s in its SQL. The lowering
                // was right the whole time (`normalize::value` takes `args.get(1)` as
                // the left operand, for the same reason); only the type rule
                // disagreed, and a disagreement between the two is the shape that
                // produces a plausible answer to the wrong question.
                // The answer *is* the piped operand's row — the same variable, not one
                // unified with it. Two fresh variables unified with each other bind in
                // whichever direction `unify` happens to take, which is how the output
                // row ended up following the argument instead.
                let (piped, argument) = (self.fresh(), self.fresh());
                self.pending.push((
                    Cons::Set {
                        left: piped.clone(),
                        right: argument.clone(),
                    },
                    sp,
                ));
                fun(query(argument), fun(query(piped.clone()), query(piped)))
            }
            Group => {
                let (r, a) = (self.fresh(), self.fresh());
                fun(
                    expr(con("row"), r.clone(), a.clone()),
                    expr(con("agg"), r, a),
                )
            }
            // `rollup e` / `cube e`: a grouping key whose column can be NULL,
            // because the subtotal rows it adds leave it out. The type says so —
            // `maybe a` out of `a` — which is what keeps the row honest: a
            // caller cannot read the key as `a` and silently get a NULL. The
            // same two arms for both, because the shape of the grouping is not in
            // the type: it is a property of the stage, and the lowerer reads it
            // off these markers.
            Rollup | Cube => {
                let (r, a) = (self.fresh(), self.fresh());
                fun(
                    expr(con("row"), r.clone(), a.clone()),
                    expr(con("agg"), r, con_of("maybe", vec![a])),
                )
            }
            Asc | Desc => {
                // `asc .x`: a row-phase expression becomes a sort key. Taking
                // an `expr` rather than a `sortkey` rejects `asc (desc .x)`.
                let (r, a) = (self.fresh(), self.fresh());
                fun(expr(con("row"), r.clone(), a), sortkey(r))
            }
            // `nullsFirst (asc .x)`: a sort key, with the NULLs moved. Taking a
            // `sortkey` rather than an `expr` is what makes the direction
            // mandatory — `nullsFirst .x` has no meaning, because where NULLs go
            // depends on the direction the default already chose.
            NullsFirst | NullsLast => {
                let r = self.fresh();
                fun(sortkey(r.clone()), sortkey(r))
            }
            // `arrayAggBy [asc .ts] .id`: a key list, then the value. The result
            // is `list a` in `agg` phase, like `arrayAgg` — the difference is the
            // order the keys give, which is the whole point.
            // `rename "to" "from" q`:
            //   `symbol ?to -> symbol ?from -> query ?r -> query (keyMap (renameSym ?to ?from) ?r)`
            //
            // The row rule is a `KeyMap`, so the output row is a type-level term
            // the signature names — the same shape `prefix`/`suffix` use, and the
            // reason `rename` is a stage the *checker* understands rather than a
            // macro only the normalizer could expand.
            Rename => {
                let (to, from, r) = (self.fresh(), self.fresh(), self.fresh_row());
                let mapper = Ty::Con("labelRename", vec![from.clone(), to.clone()]);
                let out = Ty::MapKey(Box::new(mapper), Box::new(r.clone()));
                fun(
                    Ty::Con("keyLabel", vec![to]),
                    fun(Ty::Con("keyLabel", vec![from]), fun(query(r), query(out))),
                )
            }
            ArrayAggBy => {
                let (r, a) = (self.fresh(), self.fresh());
                fun(
                    list(sortkey(r.clone())),
                    fun(
                        expr(con("row"), r.clone(), a.clone()),
                        expr(con("agg"), r, list(a)),
                    ),
                )
            }
            Rows | Range | Groups => fun(con("bound"), fun(con("bound"), con("frame"))),
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
                        if self.probe.is_some()
                            && msg.kind == crate::workspace::DiagKind::UnknownColumn
                        {
                            progress = true;
                            continue;
                        }
                        self.pending = keep;
                        return Err(TyErr {
                            span: sp,
                            kind: msg.kind,
                            msg: msg.message,
                            suggestions: self.suggested.take().into_iter().collect(),
                        });
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
    pub(crate) fn step(&mut self, c: &Cons, sp: Span) -> Result<bool, Fail> {
        match c {
            Cons::Overload { .. } => self.step_overload(c, sp),
            Cons::Filter { .. } => self.step_filter(c, sp),
            Cons::Project { .. } => self.step_project(c, sp),
            Cons::JoinOn { .. } => self.step_join_on(c, sp),
            Cons::Set { .. } => self.step_set(c, sp),
            Cons::Lit { .. } => self.step_lit(c, sp),
            Cons::Ascribe { .. } => self.step_ascribe(c, sp),
            Cons::Within { .. } => self.step_within(c, sp),
            Cons::Update { .. } => self.step_update(c, sp),
            Cons::Omit { .. } => self.step_omit(c, sp),
            Cons::MapValue { .. } => self.step_map_value(c, sp),
            Cons::Merge { .. } => self.step_merge(c, sp),
            Cons::JoinOut { .. } => self.step_join_out(c, sp),
        }
    }

    fn step_overload(&mut self, c: &Cons, sp: Span) -> Result<bool, Fail> {
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
        // While the *result* is still unknown, which form is wanted depends on
        // the context: `just "x"` is the value form in `v : maybe string = …`
        // and the row form inside `(.email == just "x")`, where the comparison
        // expects `expr ?r (maybe string)`. Waiting lets the context decide; a
        // definition that never pins the result is finalized by `prefer`.
        if self.result_is_open(target) && self.results_disagree(*module, &fits) {
            return Ok(false);
        }
        // Several candidates can fit while an argument is still open; rank
        // them by the shape they expect rather than by declaration order.
        let fits = self.prefer(*module, &fits, target, true);
        match fits.as_slice() {
            [] => Err(self.no_overload_at(*module, name, cands, target, sp)),
            [i] => {
                self.choose(*module, *i, target, origin, name, sp)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Commit to candidate `i` of an overload constraint: instantiate its
    /// scheme, unify it with the target, and record the choice for
    /// elaboration.
    fn choose(
        &mut self,
        module: usize,
        i: usize,
        target: &Ty,
        origin: &Origin,
        name: &str,
        sp: Span,
    ) -> U {
        let s = self.cand_scheme(module, i);
        if s.failed {
            return Err(self.failed_use(module, i));
        }
        let t = self.instantiate(&s, sp, None)?;
        self.unify(&t, target)?;
        let Origin::Site(site) = *origin else {
            return Err(Fail::from(format!(
                "internal error: unresolved hole of `{}`",
                op_name(name)
            )));
        };
        let (use_site, k) = decode(site);
        let key = *self.active.last().expect("solving inside a definition");
        self.record(key, use_site, k, Choice::Def(module, i));
        Ok(())
    }

    /// Do the surviving candidates answer with different kinds of result — a
    /// row-indexed expression on one side, a plain value on the other?
    ///
    /// That is the value/row twin pair, and while the context has not said
    /// which it wants, choosing is a guess: `just "x"` is a value on its own
    /// and a row expression inside `(.email == just "x")`.
    /// Whether the candidates that fit disagree about the result being a row
    /// expression or a plain value.
    ///
    /// When they do, the *context* decides which form is wanted (`just "x"` is
    /// the value form in `v : maybe string = …` and the row form inside
    /// `(.email == just "x")`), so the constraint waits. Ranking the candidates
    /// here instead — which an earlier version did, to stop a value twin from
    /// deferring a decision — cost enough to show up in the scaling test
    /// (`perf::the_pipeline_is_not_exponential`), and it was solving the wrong
    /// problem: the window-spec case it was written for turned out to be a
    /// sort-key confusion in `winspec`, fixed where it belongs.
    fn results_disagree(&mut self, module: usize, fits: &[usize]) -> bool {
        let mut expr = false;
        let mut value = false;
        for &i in fits {
            let s = self.cand_scheme(module, i);
            let mut t = s.ty.clone();
            loop {
                match self.resolve(&t) {
                    Ty::Fun(_, r) => t = (*r).clone(),
                    tail => {
                        if matches!(tail, Ty::Con("expr", _)) {
                            expr = true;
                        } else {
                            value = true;
                        }
                        break;
                    }
                }
            }
        }
        expr && value
    }

    /// Is the tail of this arrow spine still a variable?
    fn result_is_open(&self, t: &Ty) -> bool {
        let mut t = self.resolve(t);
        loop {
            match t {
                Ty::Fun(_, r) => t = self.resolve(&r),
                Ty::Var(_) => return true,
                _ => return false,
            }
        }
    }

    fn step_filter(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::Filter { pred, row } = c else {
            unreachable!("dispatched on Cons::Filter")
        };
        match self.resolve(pred) {
            Ty::Var(_) => Ok(false),
            Ty::Con("bool", _) => Ok(true),
            Ty::Con("expr", a) => {
                let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                if is_join(&self.resolve(&r)) {
                    return Err(Fail::from(rules::JOIN_ONLY));
                }
                self.place(Place::Where, &p)?;
                if self.row_open(row) {
                    return Ok(false);
                }
                self.unify(row, &r)?;
                // `bool` or `maybe bool`. SQL's `WHERE` already drops a NULL
                // predicate, so a three-valued condition is exactly what this
                // stage means; requiring `bool` forced every filter over a
                // nullable column — or over an aggregate, which is `maybe` —
                // to spell a `coalesce` sentinel, and the sentinel changes
                // which rows survive (`SUM` NULL vs `COALESCE(SUM, 0)`).
                let v = self.resolve(&v);
                match &v {
                    // The predicate's own type is still open — the row it reads
                    // may not have closed yet (`where .ok` over a column the
                    // annotation declares `maybe bool`). Defaulting it to
                    // `bool` here would then contradict that column, so wait.
                    Ty::Var(_) => return Ok(false),
                    Ty::Con("bool", _) => {}
                    Ty::Con("maybe", a) if matches!(self.resolve(&a[0]), Ty::Con("bool", _)) => {}
                    _ => {
                        return Err(Fail::from(format!(
                            "`where` needs a bool or maybe bool condition, found {}",
                            self.show(&v)
                        )))
                    }
                }
                Ok(true)
            }
            o => Err(Fail::from(format!(
                "`where` needs a bool condition, found {}",
                self.show(&o)
            ))),
        }
    }

    fn step_project(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
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
                return Err(Fail::from(msg));
            }
        }
        if fs.is_empty() {
            return Err(Fail::from(format!("`{stage}` needs at least one field")));
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
                // A scalar is a constant expression, and so is a list: a
                // `select { xs = [1, 2] }` field is a value, not a nullary
                // function, and the lowering spells it as an array literal.
                Ty::Con(s, sa) if sa.is_empty() && SCALARS.contains(&s) => out.push((l, t)),
                Ty::Con("list", _) => out.push((l, t)),
                Ty::Con("expr", a) => {
                    let (p, r, v) = (a[0].clone(), a[1].clone(), a[2].clone());
                    // A projection's field is a `select`/`agg` field, so a
                    // side-qualified name is the join-predicate mistake.
                    if is_join(&self.resolve(&r)) {
                        return Err(Fail::kinded(
                            crate::workspace::DiagKind::JoinSide,
                            format!("field `{l}`: {}", rules::JOIN_ONLY),
                        ));
                    }
                    self.stage_phase(&p, *agg)
                        .map_err(|m| m.prefixed(&format!("field `{l}` ")))?;
                    if let Err(m) = self.unify(input, &r) {
                        // A nullable mismatch names the whole value, and the
                        // field's own span is what a rewrite must touch — so
                        // the suggestion is built here, where the message and
                        // that span meet.
                        if let Some(field) = self
                            .field_spans
                            .get(&(_sp.start, _sp.end))
                            .and_then(|fs| fs.iter().find(|(n, _)| *n == l))
                            .map(|(_, sp)| *sp)
                        {
                            // The value to rewrite is the `maybe` inside the
                            // field, not the field: `upper .s` is a `string`,
                            // `.s` is the `maybe string` the position refused.
                            // The message names the value's column: the outer
                            // prefix is the projection's field, the inner one
                            // is the column the record literal wrote.
                            let column = m
                                .message
                                .match_indices("field `")
                                .filter_map(|(i, _)| {
                                    let tail = &m.message[i + 7..];
                                    tail.find('`').map(|j| (&tail[..j], i))
                                })
                                .last()
                                .map(|(n, _)| n.to_string())
                                .unwrap_or_else(|| l.clone());
                            let inner = self
                                .field_uses
                                .iter()
                                .filter(|(n, sp)| {
                                    *n == column && sp.start >= field.start && sp.end <= field.end
                                })
                                .map(|(_, sp)| *sp)
                                .min_by_key(|sp| sp.end - sp.start)
                                .unwrap_or(field);
                            self.suggested = crate::check::ty::nullable_fix(&m.message, inner)
                                .into_iter()
                                .next();
                        }
                        // The message may already name a field, and it stays: the
                        // language server reads the *innermost* name to decide what
                        // to wrap (`nullable_wrap` takes the last `field `…`` in
                        // the message), so `field `x`: field `s`: …` is a contract
                        // between the two crates and not just a chain of labels.
                        // It does read as two problems, which is worth fixing — but
                        // only together with the LSP rule, not before it.
                        return Err(m.prefixed(&format!("field `{l}`: ")));
                    }
                    out.push((l, v));
                }
                o => {
                    let msg = format!("field `{l}` of `{stage}` must be a column expression or constant, found {}", self.show(&o));
                    return Err(Fail::from(msg));
                }
            }
        }
        self.unify(&row(out, Ty::Empty), output)?;
        Ok(true)
    }

    fn step_join_on(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::JoinOn { pred, left, right } = c else {
            unreachable!("dispatched on Cons::JoinOn")
        };
        let (p, r, v) = match self.resolve(pred) {
            Ty::Var(_) => return Ok(false),
            Ty::Con("bool", _) => return Ok(true),
            Ty::Con("expr", a) => (a[0].clone(), a[1].clone(), a[2].clone()),
            o => {
                return Err(Fail::kinded(
                    crate::workspace::DiagKind::NotACondition,
                    format!(
                        "a join predicate must be a bool expression, found {}",
                        self.show(&o)
                    ),
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
                return Err(Fail::from(rules::needs_side(n)));
            }
        }
        // A join predicate is a condition, so a `maybe bool` is as valid as a
        // `bool` — and it is the *only* way to join on a nullable key.
        //
        // SQL's own rule is that `NULL = anything` never matches, which is
        // exactly what a join on a nullable key wants: the unmatched rows stay
        // unmatched. The ordinary expression `==` still requires two non-null
        // operands, because there a NULL result silently drops a row from a
        // `where`; a join predicate is a different position with a different
        // meaning, so it is the position that changes rather than the operator.
        //
        // Before this, a three-table left join whose second key had gone nullable
        // could not be written at all: the natural spelling was refused, and the
        // only suggestion was `?? 0`, which emits `COALESCE(key, 0)` and matches
        // a NULL-extended row to a real row whose key is 0 — a wrong answer that
        // looks right.
        let is_bool = self.unify(&v, &con("bool")).is_ok();
        if !is_bool {
            let maybe_bool = Ty::Con("maybe", vec![con("bool")]);
            self.unify(&v, &maybe_bool).map_err(|_| {
                Fail::kinded(
                    crate::workspace::DiagKind::NotACondition,
                    format!("a join predicate must be bool, found {}", self.show(&v)),
                )
            })?;
        }
        Ok(true)
    }

    /// A set operation's two operands must have the same columns — by **name**, not
    /// by position — and the answer keeps the **left** operand's order.
    ///
    /// Aligning by name is what makes `{ id, name }` and `{ name, id }` the same row
    /// written differently, which they are: a set operation is about which rows
    /// appear, and a row's columns are named. Requiring the same order made the user
    /// write a `select` on one branch purely to reorder, which says nothing about the
    /// query.
    ///
    /// The *left* operand is the one written first in the source — `a & union b` — so
    /// the answer is `a`'s order. That is what the lowering already emits (it takes
    /// the piped argument as the left input) and what the arena already answers with;
    /// this makes the type agree with both.
    ///
    /// Each column still has to have the same type on both sides, which is the part
    /// that is about meaning rather than arrangement.
    fn step_set(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::Set { left, right } = c else {
            unreachable!("dispatched on Cons::Set")
        };
        if self.row_open(left) || self.row_open(right) {
            // The shapes are not known yet; `solve` re-runs this once they are.
            return Ok(false);
        }
        let (lf, _) = self.flatten(left);
        let (rf, _) = self.flatten(right);
        let mut left_names: Vec<&str> = lf.iter().map(|(n, _)| n.as_str()).collect();
        let mut right_names: Vec<&str> = rf.iter().map(|(n, _)| n.as_str()).collect();
        left_names.sort_unstable();
        right_names.sort_unstable();
        if left_names != right_names {
            return Err(Fail::from(format!(
                "set-operation inputs must have the same columns: left has [{}], right has                  [{}]",
                lf.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
                rf.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        // Each name's type must agree. Matched by name, so the two orders do not have
        // to line up.
        for (name, lty) in &lf {
            let Some((_, rty)) = rf.iter().find(|(n, _)| n == name) else {
                unreachable!("the name sets are equal")
            };
            self.unify(lty, rty)
                .map_err(|m| format!("set-operation column `{name}`: {m}"))?;
        }
        Ok(true)
    }

    fn step_lit(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::Lit { lit, target } = c else {
            unreachable!("dispatched on Cons::Lit")
        };
        match self.resolve(target) {
            Ty::Var(_) => Ok(false),
            _ => self.lift(lit, target).map(|_| true),
        }
    }

    /// `(e : ty)` holds once `e`'s row is known.
    ///
    /// Waiting is the whole point: `(.a : int)` is `expr ?r ?a` against `int`, and
    /// `?r` is pinned by the stage the ascription sits in. A row that is still open
    /// means the enclosing stage has not been resolved yet, so this is retried
    /// rather than reported.
    fn step_ascribe(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::Ascribe { inner, want } = c else {
            unreachable!("dispatched on Cons::Ascribe")
        };
        // An expression's type is `expr ?phase ?row ?a`, and the annotation is
        // about `?a` — the value's type. The row and the phase are properties of
        // *where the expression was written*, not of what the ascription says, so
        // they are not part of the check: `(.a : int)` constrains the column's
        // type, and the stage still decides the row.
        //
        // This is what makes the check happen at all. Unifying the whole
        // `expr …` against `int` compares a row against a scalar, which either
        // fails on a well-typed program or — worse — waits forever for a row that
        // the ascription itself is not asking about.
        let subject = match self.resolve(inner) {
            Ty::Con("expr", args) if args.len() == 3 => args[2].clone(),
            other => other,
        };
        self.unify(&subject, want).map(|_| true)
    }

    fn step_within(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
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
    fn step_update(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
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
                return Err(Fail::from(format!(
                    "`update` expects a record of column expressions, found {}",
                    self.show(fields)
                )))
            }
        }
        if fs.is_empty() {
            return Err(Fail::from("`update` needs at least one field"));
        }
        let mut seen: Vec<String> = Vec::new();
        for (n, _) in &fs {
            if seen.contains(n) {
                return Err(Fail::from(format!("field `{n}` appears twice in `update`")));
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
                        return Err(Fail::kinded(
                            crate::workspace::DiagKind::JoinSide,
                            format!("field `{l}`: {}", rules::JOIN_ONLY),
                        ));
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
                    return Err(Fail::from(format!(
                        "field `{l}` of `update` must be a column expression or \
                                 constant, found {}",
                        self.show(&o)
                    )))
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
        // `result` is the merged query row (`merge ?r ?s`).
        let updated = values
            .iter()
            .map(|(name, _, value)| (name.clone(), value.clone()))
            .collect();
        self.unify(&row(updated, Ty::Empty), output)?;
        self.unify(&row_or_tail(out, itail), result)?;
        Ok(true)
    }

    /// `omit "k"` *is* this equation, and the equation is the stage's own:
    /// `output` is `removeKey ?k input`, so it reduces exactly when the column
    /// is there. The rule belongs to `omit`'s definition and is instantiated
    /// per use, so the label arrives as the signature's `k` bound to the
    /// literal's `Sym` — never recovered from a composed result row.
    ///
    /// The reducer is what tells "not yet" from "not there", which is the
    /// difference between waiting for the row to close and reporting a missing
    /// column. Only this stage reports it, because only this stage owns it.
    fn step_omit(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::Omit { label, output, .. } = c else {
            unreachable!("dispatched on Cons::Omit")
        };
        // The label is a `symbol ?k` until the use site binds it to a literal.
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
    /// `output` is `mapValue ?w input`, so it reduces exactly when the wrapper
    /// and the row are known. The rule belongs to `mapValue`'s definition and
    /// is instantiated per use, so the wrapper arrives as the signature's `w`
    /// bound to the literal's `Sym` — never recovered from a composed row.
    ///
    /// The reducer is what tells "not yet" from "not a wrapper", which is the
    /// difference between waiting for the row to close and reporting an unknown
    /// wrapper. Only this stage reports it, because only this stage owns it.
    fn step_map_value(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
        let Cons::MapValue { label, output } = c else {
            unreachable!("dispatched on Cons::MapValue")
        };
        // The label is a `valueLabel ?w` variable until the use site binds it.
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
    /// here: the row this constraint produces and the row ?a `merge` term
    /// reduces to have to be the same rule, or `update`'s signature and a
    /// `merge` in a signature could disagree about a collision.
    fn step_merge(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
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

    fn step_join_out(&mut self, c: &Cons, _sp: Span) -> Result<bool, Fail> {
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
        // A name both inputs have is renamed, not dropped: the merged row keeps
        // the left value under its own name and the right one under
        // `right_<name>` (`rules::join_columns`). Nothing is lost and nothing is
        // silently shadowed, so there is nothing to warn about — the previous
        // version warned and dropped, which told a reader about the loss without
        // preventing it.
        let wrap = |this: &Self, on: bool, (k, t): (String, Ty)| match this.resolve(&t) {
            Ty::Con("maybe", _) => (k, t),
            _ if on => (k, Ty::Con("maybe", vec![t])),
            _ => (k, t),
        };
        let lf: Vec<_> = lf.into_iter().map(|c| wrap(self, nullable.0, c)).collect();
        let rf: Vec<_> = rf.into_iter().map(|c| wrap(self, nullable.1, c)).collect();
        let actual = row(rules::join_columns(&lf, &rf), Ty::Empty);
        // An annotated join wrapper exposes `merge ?r ?s` in its result
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
            // public type is right-biased; the SQL join retains
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
                Some(ph) => rules::place(at, ph).map_err(placement),
                None => Err(Fail::from(format!("not a phase: {}", self.show(&t)))),
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
            Ty::Var(v) if self.vars[v as usize].row_or_win => {
                rules::place(at, Phase::Row).map_err(placement)
            }
            _ => self.unify(p, &con("agg")),
        }
    }

    /// Phase rules for a `where` condition, join predicate, or key: row (or
    /// constant). An open phase becomes `row`.
    pub(crate) fn place(&mut self, at: Place, p: &Ty) -> U {
        match phase_of(&self.resolve(p)) {
            Some(ph) => rules::place(at, ph).map_err(placement),
            None => self.unify(p, &con("row")),
        }
    }
}

/// The diagnostic for a type name the language does not have.
fn unknown_type_name(name: &str) -> String {
    const KNOWN: &[&str] = &[
        "int",
        "float",
        "string",
        "bool",
        "date",
        "timestamp",
        "maybe",
        "list",
        "expr",
        "agg",
        "win",
        "query",
        "row",
    ];
    let near = KNOWN
        .iter()
        .map(|k| (*k, crate::suggest::edit_distance(name, k)))
        .filter(|(k, d)| *d <= 2 || k.starts_with(name) || name.starts_with(*k))
        .min_by_key(|(_, d)| *d);
    // The hint names the sigil, because that is the one thing a writer of a
    // generic signature has to know and the one thing the old rule got wrong: a
    // bare name is never a variable, so `?a` is how a variable is written.
    match near {
        Some((k, _)) => format!(
            "unknown type `{name}`: did you mean `{k}`? Type names are closed; a type \
             *variable* is written with a `?`, as in `?a`"
        ),
        None => format!(
            "unknown type `{name}`: type names are closed ({}); a type *variable* is \
             written with a `?`, as in `?a`",
            KNOWN.join(", ")
        ),
    }
}
