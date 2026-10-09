//! [`Checker`] substitution and unification (including row unification).
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

impl<'w> Checker<'w> {
    // ── variables and substitution ─────────────────────────────────────────

    pub(crate) fn fresh_id(&mut self, row_or_win: bool) -> u32 {
        self.fresh_with(row_or_win, false, Kind::Type)
    }

    pub(crate) fn fresh_with(&mut self, row_or_win: bool, nonnull: bool, kind: Kind) -> u32 {
        self.vars.push(VarInfo {
            bound: None,
            kind,
            row_or_win,
            nonnull,
            version: 0,
        });
        self.vars.len() as u32 - 1
    }

    pub(crate) fn fresh(&mut self) -> Ty {
        Ty::Var(self.fresh_id(false))
    }

    pub(crate) fn fresh_row(&mut self) -> Ty {
        Ty::Var(self.fresh_with(false, false, Kind::Row))
    }

    pub(crate) fn fresh_col_phase(&mut self) -> Ty {
        Ty::Var(self.fresh_id(true))
    }

    pub(crate) fn resolve(&self, t: &Ty) -> Ty {
        let mut t = t.clone();
        while let Ty::Var(v) = t {
            match &self.vars[v as usize].bound {
                Some(b) => t = b.clone(),
                None => break,
            }
        }
        t
    }

    /// Resolve a variable chain and collapse it to the first non-variable.
    /// Changes are trailed because overload fitting temporarily rolls back
    /// unification, just like ordinary bindings.
    pub(crate) fn resolve_compress(&mut self, t: &Ty) -> Ty {
        let Ty::Var(v) = t else {
            return t.clone();
        };
        let Some(bound) = self.vars[*v as usize].bound.clone() else {
            return Ty::Var(*v);
        };
        let resolved = self.resolve_compress(&bound);
        if resolved != bound {
            self.trail.push((*v, self.vars[*v as usize].clone()));
            self.vars[*v as usize].bound = Some(resolved.clone());
        }
        resolved
    }

    /// Whether a `keyMap` term survives on the *spine* of `t` once bound
    /// variables are followed. A term on the spine renames the row it is part
    /// of, so that row is still constrainable by a later signature.
    ///
    /// Only the spine is walked. A `keyMap` inside a field's *value* type
    /// describes that value, not the shape of this row, and counting it would
    /// make every row that projects a renamed row look constrainable.
    pub(crate) fn keymap_on_spine(&self, t: &Ty) -> bool {
        let mut cur = self.resolve(t);
        loop {
            match cur {
                Ty::MapKey(..) => return true,
                Ty::Row(_, tail) => cur = self.resolve(&tail),
                Ty::Merge(left, _) => cur = self.resolve(&left),
                Ty::MapValue(_, row) => cur = self.resolve(&row),
                _ => return false,
            }
        }
    }

    /// Whether a row's field list is still **incomplete**, so a stage that
    /// reads its columns has to wait for it (`Ok(false)` from a constraint
    /// step) instead of comparing against a row that may still grow.
    ///
    /// A variable is the plain case. The one that matters is an *unreduced row
    /// term* — `keyMap`, `merge`, `mapValue` — whose operands are not known
    /// yet, so its fields have not been computed. Reading such a row as final
    /// reports the columns it is about to contribute as missing:
    /// `q & update { a = … } & update { b = .a }` hands the second stage
    /// `merge α { a = int }`, whose left operand only becomes known once the
    /// helper is applied to a query, so `a` looked absent.
    ///
    /// This is the predicate `step_project` has always used; the other stages
    /// that read their input's columns ask the same question, so they now ask
    /// it the same way.
    pub(crate) fn row_open(&self, t: &Ty) -> bool {
        matches!(
            self.flatten(t).1,
            Ty::Var(_) | Ty::MapKey(..) | Ty::Merge(..) | Ty::MapValue(..)
        )
    }

    /// Row fields and the tail after following bound variables.
    pub(crate) fn flatten(&self, t: &Ty) -> (Vec<(String, Ty)>, Ty) {
        let mut fs = Vec::new();
        let mut cur = self.resolve(t);
        loop {
            match cur {
                Ty::Row(more, tail) => {
                    fs.extend(more);
                    cur = self.resolve(&tail);
                }
                Ty::MapKey(km, row) => {
                    // Resolve both components before trying to reduce. A
                    // pipeline often binds the mapper and inner row through
                    // separate applications, so leaving either child as a
                    // variable makes an otherwise closed term look open.
                    let km = self.resolve(&km);
                    let (rf, rt) = self.flatten(&row);
                    let row = row_or_tail(rf, rt);
                    if let Some(reduced) = reduce_mapkey(&km, &row) {
                        cur = reduced;
                    } else {
                        // Cannot reduce yet; return MapKey as tail
                        return (fs, Ty::MapKey(Box::new(km), Box::new(row)));
                    }
                }
                Ty::Merge(left, right) => {
                    // Resolve both operands before reducing. A join binds
                    // each side through separate constraints, so the merge
                    // often becomes closed only after this term was built.
                    let left = self.resolve(&left);
                    let right = self.resolve(&right);
                    // A row tail can itself be a row after unification
                    // (`{ id | tail }` with a closed input). Flatten those
                    // nested tails before asking the first-order reducer
                    // whether both merge operands are closed.
                    let (lf, lt) = self.flatten(&left);
                    let (rf, rt) = self.flatten(&right);
                    let left = row_or_tail(lf, lt);
                    let right = row_or_tail(rf, rt);
                    if let Some(reduced) = reduce_merge(&left, &right) {
                        cur = reduced;
                    } else {
                        // Cannot reduce yet; return Merge as tail
                        return (fs, Ty::Merge(Box::new(left), Box::new(right)));
                    }
                }
                Ty::MapValue(vm, row) => {
                    let row = self.resolve(&row);
                    if let Some(reduced) = reduce_mapvalue(&vm, &row) {
                        cur = reduced;
                    } else {
                        // Cannot reduce yet; return MapValue as tail
                        return (fs, Ty::MapValue(vm, Box::new(row)));
                    }
                }
                other => return (fs, other),
            }
        }
    }

    pub(crate) fn zonk(&self, t: &Ty) -> Ty {
        match self.resolve(t) {
            Ty::Con(n, args) => Ty::Con(n, args.iter().map(|a| self.zonk(a)).collect()),
            Ty::Fun(a, b) => fun(self.zonk(&a), self.zonk(&b)),
            r @ Ty::Row(..) => {
                let (fs, tail) = self.flatten(&r);
                let fs = fs.into_iter().map(|(k, v)| (k, self.zonk(&v))).collect();
                row_or_tail(fs, tail)
            }
            Ty::MapKey(km, row) => {
                // Resolve *both* sides before reducing. The mapper may have
                // been an unbound variable when the term was built and bound
                // since (that is how a deferred `prefix p` becomes concrete),
                // so passing the unresolved `km` here would miss the reduction.
                let km = self.zonk(&km);
                let row = self.zonk(&row);
                // The reduced form is *not* zonked recursively: a row whose
                // tail is still a `keyMap` would otherwise re-enter this arm on
                // the same term forever.
                match reduce_mapkey(&km, &row) {
                    Some(reduced) => self.zonk(&reduced),
                    None => Ty::MapKey(Box::new(km), Box::new(row)),
                }
            }
            Ty::Merge(left, right) => {
                let left = self.zonk(&left);
                let right = self.zonk(&right);
                // Try to reduce after zonking
                if let Some(reduced) = reduce_merge(&left, &right) {
                    self.zonk(&reduced)
                } else {
                    Ty::Merge(Box::new(left), Box::new(right))
                }
            }
            Ty::MapValue(vm, row) => {
                let row = self.zonk(&row);
                // Try to reduce after zonking
                if let Some(reduced) = reduce_mapvalue(&vm, &row) {
                    self.zonk(&reduced)
                } else {
                    Ty::MapValue(vm, Box::new(row))
                }
            }
            o => o,
        }
    }

    pub(crate) fn occurs(&self, v: u32, t: &Ty) -> bool {
        match self.resolve(t) {
            Ty::Var(u) => u == v,
            Ty::Con(_, args) => args.iter().any(|a| self.occurs(v, a)),
            Ty::Fun(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            Ty::Row(fs, tail) => fs.iter().any(|(_, t)| self.occurs(v, t)) || self.occurs(v, &tail),
            Ty::MapKey(_, row) => self.occurs(v, &row),
            Ty::Merge(left, right) => self.occurs(v, &left) || self.occurs(v, &right),
            Ty::MapValue(_, row) => self.occurs(v, &row),
            _ => false,
        }
    }

    // ── unification ────────────────────────────────────────────────────────

    pub(crate) fn bind(&mut self, v: u32, t: Ty) -> U {
        let t = self.resolve_compress(&t);
        if t == Ty::Var(v) {
            return Ok(());
        }
        if self.occurs(v, &t) {
            return Err(format!(
                "infinite type: a type would contain itself ({})",
                self.show(&t)
            ));
        }
        self.trail.push((v, self.vars[v as usize].clone()));
        if self.vars[v as usize].row_or_win {
            match &t {
                Ty::Var(u) => {
                    self.trail.push((*u, self.vars[*u as usize].clone()));
                    if !self.vars[*u as usize].row_or_win {
                        self.vars[*u as usize].row_or_win = true;
                        self.vars[*u as usize].version =
                            self.vars[*u as usize].version.wrapping_add(1);
                    }
                }
                Ty::Con("row" | "win", _) => {}
                Ty::Con("agg", _) => return Err(UNGROUPED.into()),
                o => {
                    return Err(format!(
                        "a column expression cannot have phase {}",
                        self.show(o)
                    ))
                }
            }
        }
        if self.vars[v as usize].nonnull {
            match &t {
                Ty::Var(u) => {
                    self.trail.push((*u, self.vars[*u as usize].clone()));
                    if !self.vars[*u as usize].nonnull {
                        self.vars[*u as usize].nonnull = true;
                        self.vars[*u as usize].version =
                            self.vars[*u as usize].version.wrapping_add(1);
                    }
                }
                Ty::Con("maybe", _) => return Err(NULLABLE.into()),
                _ => {}
            }
        }
        self.vars[v as usize].bound = Some(t);
        self.vars[v as usize].version = self.vars[v as usize].version.wrapping_add(1);
        Ok(())
    }

    /// Unify `actual` with `expected` (the order only affects messages).
    /// Reduce a `keyMap` term on either side of a unification, if it can be
    /// reduced. Returns the replacement pair, or `None` when neither side is a
    /// reducible term.
    ///
    /// This is R-MapKey-Closed/Compose applied at the point where the row
    /// becomes known: a key stage may have been registered against an
    /// unresolved row and retained its term, and the binding that completes the
    /// row happens during a later unification.
    ///
    /// The term's stored row is resolved first, because it was captured when
    /// the stage ran, which may have been before the row's tail was bound. That
    /// resolution is what turns a retained `keyMap m r` into a reducible one.
    pub(crate) fn reduce_mapkey_pair(&mut self, a: &Ty, e: &Ty) -> Option<(Ty, Ty)> {
        match (a, e) {
            // Two `keyMap` terms unify structurally, not by reduction: their
            // mappers and rows are unified in turn, which is what lets an
            // unbound mapper variable adopt a concrete witness. Only a `keyMap`
            // facing something *other* than a `keyMap` is a reduction.
            (Ty::MapKey(..), Ty::MapKey(..)) => None,
            (Ty::MapKey(km, row), other) => {
                let reduced = self.reduce_mapkey_row(km, row)?;
                Some((reduced, other.clone()))
            }
            (other, Ty::MapKey(km, row)) => {
                let reduced = self.reduce_mapkey_row(km, row)?;
                Some((other.clone(), reduced))
            }
            _ => None,
        }
    }

    /// Reduce one `keyMap` term to the row it denotes, or `None` while the
    /// mapper or the row is still open.
    ///
    /// A row that is already closed reduces exactly as it stands, and must: its
    /// shape may be an annotation's, deliberately narrower than the column set
    /// a later stage binds.
    ///
    /// The remaining case is a row left open by a stage that narrowed it —
    /// `{first, … | tail}` with the tail bound afterwards — or a *chained* key
    /// stage, whose term's inner row is another `keyMap` term. `resolve` follows
    /// only a top-level variable, so such a row still looked open, the term was
    /// judged unreduced, and the rewrite was silently lost or — worse — left
    /// uncompared: `users & order [asc .id] & prefix "lit_"` reached the next
    /// stage unrenamed and the projection after it was left with an
    /// unconstrained row, and `users & prefix "u_" & suffix "_v2"` skipped its
    /// signature check entirely. `flatten` follows both shapes, so it is what
    /// decides reducibility here. It is only consulted when the shallow form did
    /// not reduce, so a closed row is still never re-normalized into a wider
    /// one.
    pub(crate) fn reduce_mapkey_row(&mut self, km: &Ty, row: &Ty) -> Option<Ty> {
        // Only a *concrete* mapper can reduce. A variable mapper has no
        // transformation to apply, and trying anyway is how a deferred term
        // turns into a cycle.
        let km = self.resolve(km);
        KeyMap::of_ty(&km)?;
        let shallow = self.resolve(row);
        let reduced = match reduce_mapkey(&km, &shallow) {
            Some(reduced) => reduced,
            None => {
                // `resolve` follows only a top-level variable, so a row that is
                // still open *inside* — a chained key stage's `keyMap m r` whose
                // `r` has been bound since, or a narrowed `{ … | tail }` — reads
                // as open here even though it has become reducible. `flatten`
                // does follow those, and hands back the term itself as the tail
                // when it still cannot reduce, so it decides reducibility for
                // both shapes.
                //
                // This must not be narrowed to the `{ … | tail }` case alone:
                // for a chained stage the misread left `unify_inner` taking its
                // "not reducible yet" path, which returns `Ok(())` and skips the
                // comparison entirely — so a signature naming a column the
                // pipeline never produces (`query { bogus = int }` over `users &
                // prefix "u_" & suffix "_v2"`) was accepted unchallenged.
                let (fs, tail) = self.flatten(row);
                reduce_mapkey(&km, &row_or_tail(fs, tail))?
            }
        };
        if matches!(&reduced, Ty::MapKey(..)) {
            return None;
        }
        Some(reduced)
    }

    pub(crate) fn unify(&mut self, actual: &Ty, expected: &Ty) -> U {
        // A `keyMap` term can reduce into another `keyMap` term, so unification
        // needs a depth bound: without one, a cyclic reduction overflows the
        // stack instead of reporting a type error. The bound is generous enough
        // that no real term reaches it.
        const MAX_UNIFY_DEPTH: usize = 512;
        if self.unify_depth >= MAX_UNIFY_DEPTH {
            return Err("type is too deeply nested to unify; a `keyMap` term may \
                        reduce in a cycle"
                .into());
        }
        self.unify_depth += 1;
        let r = self.unify_inner(actual, expected);
        self.unify_depth -= 1;
        r
    }

    pub(crate) fn unify_inner(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (a, e) = (
            self.resolve_compress(actual),
            self.resolve_compress(expected),
        );
        // Kind check before unifying.
        //
        // A *variable* has not committed to a kind yet: `self.fresh()` creates
        // an unconstrained one, and it is the use that decides what it is. So a
        // variable adopts the other side's kind instead of clashing with it —
        // which is what lets a lambda parameter be inferred as a row, as in
        // `_&=_ = q => fields => select fields q`. Only two *concrete* types of
        // different kinds are a genuine error.
        let kind_a = kind_of(&a, &self.vars);
        let kind_e = kind_of(&e, &self.vars);
        if kind_a != kind_e {
            match (&a, &e) {
                // Two uncommitted variables: neither has decided, so adopt the
                // more specific kind and let the binding carry it. Reached when
                // an inferred lambda parameter is later used as a row.
                (Ty::Var(v), Ty::Var(w)) => {
                    let kind = if kind_a == Kind::Type { kind_e } else { kind_a };
                    self.vars[*v as usize].kind = kind;
                    self.vars[*w as usize].kind = kind;
                }
                (Ty::Var(v), _) => {
                    self.vars[*v as usize].kind = kind_e;
                }
                (_, Ty::Var(v)) => {
                    self.vars[*v as usize].kind = kind_a;
                }
                _ => {
                    return Err(format!(
                        "kind mismatch: cannot unify {:?} with {:?}",
                        kind_a, kind_e
                    ))
                }
            }
        }
        // R-MapKey-Closed: `keyMap m row` reduces as soon as `row` is known.
        //
        // A key stage is registered while its input may still be an unresolved
        // variable, in which case the term is retained (R-MapKey-Open). The
        // binding that completes the row happens later, so the reduction has to
        // be attempted here, when either side of a unification still carries a
        // reducible term. The term's stored row is resolved first, since it was
        // captured before the tail was bound.
        match (&a, &e) {
            (Ty::Var(v), _) => self.bind(*v, e.clone()),
            (_, Ty::Var(v)) => self.bind(*v, a.clone()),
            (Ty::Rigid(x, _), Ty::Rigid(y, _)) if x == y => Ok(()),
            (Ty::Empty, Ty::Empty) => Ok(()),
            // Two affixes unify when they are the same constructor applied to
            // the same literal — `prefix "u_"` with `prefix "u_"`. Different
            // affixes are a genuine mismatch, reported as such.
            (Ty::KeyAffix(ca, sa), Ty::KeyAffix(cb, sb)) if ca == cb && sa == sb => Ok(()),
            // Two symbolic row terms unify componentwise. This must be checked
            // before the one-sided reduction arm below: otherwise the wildcard
            // would swallow this case and leave the mapper variables unrelated.
            (Ty::MapKey(km_a, row_a), Ty::MapKey(km_b, row_b)) => {
                let (ka, kb) = (km_a.as_ref().clone(), km_b.as_ref().clone());
                let (ra, rb) = (row_a.as_ref().clone(), row_b.as_ref().clone());
                self.unify(&ka, &kb)?;
                self.unify(&ra, &rb)
            }
            // Open row formers must unify structurally. Sending two
            // unresolved `merge`/`mapValue` terms through `unify_rows` would
            // flatten each one back to itself and recurse forever.
            (Ty::Merge(al, ar), Ty::Merge(el, er)) => {
                let (al, ar, el, er) = (al.clone(), ar.clone(), el.clone(), er.clone());
                self.unify(&al, &el)?;
                self.unify(&ar, &er)
            }
            (Ty::MapValue(av, ar), Ty::MapValue(ev, er)) if av == ev => {
                let (ar, er) = (ar.clone(), er.clone());
                self.unify(&ar, &er)
            }
            // A `keyMap` term facing a concrete row reduces first, so a stage's
            // `query (keyMap (prefix "u_") s)` and an annotation's
            // `query { u_id = … }` are the same type. This must come before the
            // `unify_rows` arm: flattening there would compare the term's *inner*
            // (unrenamed) row instead of the row the rewrite produces.
            (Ty::MapKey(..), _other) | (_other, Ty::MapKey(..)) => {
                let (ra, re) = (a.clone(), e.clone());
                match self.reduce_mapkey_pair(&ra, &re) {
                    // Reduced: unify the row the rewrite produces.
                    Some(pair) if pair.0 != ra || pair.1 != re => self.unify(&pair.0, &pair.1),
                    // Not reducible *yet*: the mapper or the row is still open.
                    //
                    // Comparing the term's inner row here would be wrong — that
                    // row has not been renamed — and it is what produced "no
                    // column `u_id`" for a stage that was about to be correct.
                    // Leaving the comparison satisfied-and-pending is safe: the
                    // stage registered a `Cons::MapKey` equation, so `solve`
                    // retries once the mapper and the row are both known, and
                    // any genuine mismatch is reported then.
                    _ => Ok(()),
                }
            }
            (
                Ty::Row(..),
                Ty::Row(..) | Ty::Empty | Ty::Rigid(..) | Ty::Merge(..) | Ty::MapValue(..),
            )
            | (Ty::Empty | Ty::Rigid(..) | Ty::Merge(..) | Ty::MapValue(..), Ty::Row(..))
            | (Ty::Merge(..), Ty::Empty | Ty::Rigid(..) | Ty::MapValue(..))
            | (Ty::MapValue(..), Ty::MapValue(..) | Ty::Merge(..) | Ty::Empty | Ty::Rigid(..)) => {
                self.unify_rows(&a, &e)
            }
            (Ty::Con(x, xs), Ty::Con(y, ys)) if x == y && xs.len() == ys.len() => {
                for (p, q) in xs.clone().iter().zip(ys.clone().iter()) {
                    self.unify(p, q)?;
                }
                Ok(())
            }
            // Directional affix parameters are strings at the surface. The
            // mapper they carry is metadata used to type the result row.
            (Ty::Con("prefixAffix" | "suffixAffix", _), Ty::Con("string", _))
            | (Ty::Con("string", _), Ty::Con("prefixAffix" | "suffixAffix", _)) => Ok(()),
            (Ty::Con(ca, am), Ty::Con(cb, bm))
                if matches!(*ca, "prefixAffix" | "suffixAffix")
                    && matches!(*cb, "prefixAffix" | "suffixAffix")
                    && ca == cb
                    && am.len() == 1
                    && bm.len() == 1 =>
            {
                self.unify(&am[0], &bm[0])
            }
            (Ty::Fun(a1, r1), Ty::Fun(a2, r2)) => {
                let (a1, r1, a2, r2) = (a1.clone(), r1.clone(), a2.clone(), r2.clone());
                self.unify(&a1, &a2)?;
                self.unify(&r1, &r2)
            }
            _ => Err(self.mismatch(&a, &e)),
        }
    }

    pub(crate) fn unify_rows(&mut self, actual: &Ty, expected: &Ty) -> U {
        let (fa, ta) = self.flatten(actual);
        let (fe, te) = self.flatten(expected);
        for (l, t) in &fa {
            if let Some((_, u)) = fe.iter().find(|(k, _)| k == l) {
                self.unify(t, u).map_err(|m| format!("field `{l}`: {m}"))?;
            }
        }
        let only_a: Vec<(String, Ty)> = fa
            .iter()
            .filter(|(l, _)| !fe.iter().any(|(k, _)| k == l))
            .cloned()
            .collect();
        let only_e: Vec<(String, Ty)> = fe
            .iter()
            .filter(|(l, _)| !fa.iter().any(|(k, _)| k == l))
            .cloned()
            .collect();
        let closed = |t: &Ty| !self.row_open(t);
        if let Some((l, _)) = only_a.first() {
            if closed(&te) {
                return if self.probe.is_some() {
                    Ok(())
                } else {
                    Err(missing(l, &fe))
                };
            }
        }
        if let Some((l, _)) = only_e.first() {
            if closed(&ta) {
                return if self.probe.is_some() {
                    Ok(())
                } else {
                    Err(missing(l, &fa))
                };
            }
        }
        // The tails decide where leftover fields go, and only a *variable* tail
        // can take them: binding `τ` to `{ fields | τ }` is the row equation
        // for a stage whose input row is not known yet. A tail that is still an
        // unreduced row term cannot absorb fields here, and deciding now would
        // be reading an unfinished row as final — the mistake `closed` above
        // refuses to make. Leaving the comparison satisfied-and-pending is safe
        // for the same reason the `keyMap` arm above does: the stage registered
        // a constraint over this row, so `solve` re-runs the comparison once the
        // term reduces, and any genuine mismatch is reported then.
        let absorb = |t: &Ty| matches!(t, Ty::Var(_));
        match (only_a.is_empty(), only_e.is_empty()) {
            (true, true) => self.unify(&ta, &te),
            (false, true) if absorb(&te) => self.unify(&te, &row(only_a, ta)),
            (true, false) if absorb(&ta) => self.unify(&ta, &row(only_e, te)),
            (false, false) if absorb(&ta) && absorb(&te) => {
                if ta == te {
                    return Err("incompatible row types".into());
                }
                let tail = self.fresh_row();
                self.unify(&ta, &row(only_e, tail.clone()))?;
                self.unify(&te, &row(only_a, tail))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn mismatch(&self, a: &Ty, e: &Ty) -> String {
        if let (Some(x), Some(y)) = (phase_of(a), phase_of(e)) {
            // An `agg` / `win` where a row expression is expected is an
            // argument that nests one (a plain `expr` parameter, or the
            // argument of an aggregate or window function).
            return match (y, x) {
                (Phase::Row, Phase::Agg | Phase::Win) => {
                    rules::nested("this argument", x).unwrap_err()
                }
                _ => rules::clash(x, y),
            };
        }
        let join = |t: &Ty| matches!(t, Ty::Con("join", _));
        let plain = |t: &Ty| matches!(t, Ty::Row(..) | Ty::Empty);
        if (join(a) && plain(e)) || (plain(a) && join(e)) {
            return "cannot mix join-side columns (`.<x`, `.>x`) with plain columns (`.x`)".into();
        }
        let maybe = |t: &Ty| matches!(t, Ty::Con("maybe", _));
        let hint = if maybe(a) != maybe(e) {
            "; only one side is nullable: `coalesce default x` takes a `maybe`, `just x` makes one"
        } else {
            ""
        };
        format!(
            "type mismatch: expected {}, found {}{hint}",
            self.show(e),
            self.show(a)
        )
    }
}
