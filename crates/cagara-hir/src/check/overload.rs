//! [`Checker`] overload fitting and the choices recorded for source elaboration.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

impl<'w> Checker<'w> {
    // ── overloads ──────────────────────────────────────────────────────────

    /// Scheme of an overload candidate: checked if possible, else (inside
    /// its own body) its signature.
    pub(crate) fn cand_scheme(&mut self, m: usize, i: usize) -> Scheme {
        if let Some(s) = self.def_scheme(m, i) {
            return s;
        }
        let ann = if m == self.module {
            self.defs.get(i).ty.as_ref().map(|t| self.annotation(t))
        } else {
            None
        };
        match ann {
            Some(Ok(t)) => self.generalize(&t, vec![]),
            _ => {
                let v = self.fresh();
                self.generalize(&v, vec![])
            }
        }
    }

    pub(crate) fn cand_shown(&mut self, m: usize, i: usize) -> String {
        let s = self.cand_scheme(m, i);
        self.show_scheme(&s)
    }

    /// Would candidate type `t` unify with `target`? Leaves no trace.
    pub(crate) fn fits(&mut self, s: &Scheme, target: &Ty) -> bool {
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let t = self.inst(&s.ty, &mut HashMap::new(), &s.gens);
        let ok = self.unify(&t, target).is_ok();
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        ok
    }

    /// Where a candidate disagrees with the use, in words.
    ///
    /// `no overload of `sum` matches …; candidates: <three signatures>` makes a
    /// reader compare three signatures against one use by hand. This walks the
    /// first place each candidate and the use differ, so the message says which
    /// one is close and how — the shape a compiler's `expected … found …` has,
    /// per candidate.
    ///
    /// The trial runs on the checker's own state and is rolled back, so asking
    /// costs nothing and changes nothing.
    pub(crate) fn divergence(&mut self, m: usize, i: usize, target: &Ty) -> Option<String> {
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let s = self.cand_scheme(m, i);
        let t = self.inst(&s.ty, &mut HashMap::new(), &s.gens);
        // Unify as far as it goes; the parts that did not unify are what we read.
        let _ = self.unify(&t, target);
        let found = differ(self, &t, target, 0);
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        found
    }

    pub(crate) fn fitting(&mut self, m: usize, cands: &[usize], target: &Ty) -> Vec<usize> {
        let fingerprint = self.fingerprint(target);
        let key = (m, cands.to_vec(), fingerprint);
        if let Some(fits) = self.fit_cache.get(&key) {
            return fits.clone();
        }
        let mut out = Vec::new();
        for &i in cands {
            let s = self.cand_scheme(m, i);
            if self.fits(&s, target) {
                out.push(i);
            }
        }
        self.fit_cache.insert(key, out.clone());
        out
    }

    /// Rank the candidates that all fit, by how well their parameters match
    /// the shape of the arguments.
    ///
    /// Two overload families overlap on an *open* type: the value/row twins
    /// (`upper : string -> string` beside `upper : expr r string -> expr r string`)
    /// and the nullable twins (`a -> bool` beside `maybe a -> maybe bool`).
    /// Both fit while an argument's type is still a variable, and choosing by
    /// declaration order makes the answer depend on where the overload was
    /// written — after which the real argument type contradicts it:
    ///
    /// ```text
    /// addDays (-7) .d        -- `.d` is a row expression, not a value
    /// coalesce 0.0 .revenue  -- `.revenue` is `expr r _`
    /// ```
    ///
    /// So a candidate scores for matching the argument's phase (`expr` for a row
    /// expression, a value type for a constant) and for *not* introducing
    /// `maybe` where the argument has none — the language's default. The
    /// highest score wins; a tie is still `ambiguous use`, never a guess.
    pub(crate) fn prefer(
        &mut self,
        module: usize,
        fits: &[usize],
        target: &Ty,
        decisive_only: bool,
    ) -> Vec<usize> {
        if fits.len() < 2 {
            return fits.to_vec();
        }
        let mut scored: Vec<(i32, usize)> = Vec::with_capacity(fits.len());
        for &i in fits {
            let s = self.cand_scheme(module, i);
            let (trail, nvars) = (self.trail.len(), self.vars.len());
            let t = self.inst(&s.ty, &mut HashMap::new(), &s.gens);
            let score = self.match_score(&t, target);
            while self.trail.len() > trail {
                let (v, old) = self.trail.pop().expect("trail entry");
                if (v as usize) < nvars {
                    self.vars[v as usize] = old;
                }
            }
            self.vars.truncate(nvars);
            scored.push((score, i));
        }
        let best = scored.iter().map(|(s, _)| *s).max().unwrap_or(0);
        let winners: Vec<usize> = scored
            .iter()
            .filter(|(s, _)| *s == best)
            .map(|(_, i)| *i)
            .collect();
        // While an argument is still a variable the ranking is a *guess*, so
        // the constraint waits instead: the argument's real type may make the
        // other candidate the only one that fits (`where .ok` over a `maybe
        // bool` column). At arbitration the guess is all that is left, and
        // `decisive_only` is off.
        if decisive_only
            && winners.len() == 1
            && !self.ranked_on_resolved_arguments(module, &winners, target)
        {
            return fits.to_vec();
        }
        winners
    }

    /// Was every compared value type resolved?
    ///
    /// The nullable/non-null choice is decided by the *value* type inside a
    /// row-indexed parameter, so that is what must be known: a `where .ok`
    /// whose column type is still a variable must wait for the row to close
    /// rather than commit to the non-null signature.
    fn ranked_on_resolved_arguments(
        &mut self,
        module: usize,
        winners: &[usize],
        target: &Ty,
    ) -> bool {
        let Some(&i) = winners.first() else {
            return false;
        };
        let s = self.cand_scheme(module, i);
        let (trail, nvars) = (self.trail.len(), self.vars.len());
        let t = self.inst(&s.ty, &mut HashMap::new(), &s.gens);
        let resolved = self.any_resolved_argument(&t, target);
        while self.trail.len() > trail {
            let (v, old) = self.trail.pop().expect("trail entry");
            if (v as usize) < nvars {
                self.vars[v as usize] = old;
            }
        }
        self.vars.truncate(nvars);
        resolved
    }

    fn any_resolved_argument(&self, cand: &Ty, target: &Ty) -> bool {
        let mut c = cand.clone();
        let mut t = target.clone();
        let mut compared = false;
        while let (Ty::Fun(_, c_rest), Ty::Fun(tp, t_rest)) = (self.resolve(&c), self.resolve(&t)) {
            // Every argument the ranking looks at must be resolved, or the
            // decision is a guess: `coalesce 0 .amount` is decided by
            // `.amount`'s `maybe float`, while `where .ok` over a row that has
            // not closed yet is decided by nothing.
            compared = true;
            match self.resolve(&tp) {
                Ty::Con("expr", a) if a.len() == 3 => {
                    if matches!(self.resolve(&a[2]), Ty::Var(_)) {
                        return false;
                    }
                }
                Ty::Con(_, _) => {}
                _ => return false,
            }
            c = (*c_rest).clone();
            t = (*t_rest).clone();
        }
        compared
    }

    /// Score a candidate type against the arguments, position by position.
    pub(crate) fn match_score(&self, cand: &Ty, target: &Ty) -> i32 {
        let mut score = 0;
        let mut c = cand.clone();
        let mut t = target.clone();
        while let (Ty::Fun(cp, c_rest), Ty::Fun(tp, t_rest)) = (self.resolve(&c), self.resolve(&t))
        {
            score += self.param_score(&cp, &tp);
            c = (*c_rest).clone();
            t = (*t_rest).clone();
        }
        score
    }

    /// How well one parameter matches one argument.
    ///
    /// A row-indexed parameter (`expr r T`) is compared by its phase first, and
    /// then by the shape of `T`: a `maybe T` argument wants a `maybe T`
    /// parameter, and an argument that is *not* known to be nullable wants the
    /// non-null parameter — the language's default, so an open type is never
    /// silently decided as `maybe`.
    fn param_score(&self, cand: &Ty, arg: &Ty) -> i32 {
        let cand = self.resolve(cand);
        let arg = self.resolve(arg);
        // A row expression wants a row-indexed parameter. Comparing `T` inside
        // decides between `expr r T` and `expr r (maybe T)`.
        if let (Ty::Con("expr", c), Ty::Con("expr", a)) = (&cand, &arg) {
            if c.len() == 3 && a.len() == 3 {
                return 4 + self.value_score(&c[2], &a[2]);
            }
        }
        match (&cand, &arg) {
            // Nothing known about the argument: keep the language's defaults —
            // row-indexed when available, and non-null.
            (Ty::Con("expr", _), Ty::Var(_)) => 1,
            (Ty::Con("maybe", _), Ty::Var(_)) => -1,
            // A constant wants a value parameter, not a row-indexed one.
            (Ty::Con("expr", _), Ty::Con(_, _)) => -4,
            (Ty::Con("expr", _), _) => -3,
            (Ty::Con(_, _), Ty::Con("expr", _)) => -2,
            (Ty::Con(_, _), Ty::Var(_)) => 0,
            _ => 0,
        }
    }

    /// Compare the value types inside a row-indexed parameter.
    ///
    /// The two things that matter are "is the candidate's value type `maybe`"
    /// and "is either side still unknown". A candidate whose value type is a
    /// bare variable adapts to anything, so it is the safest reading when
    /// nothing is known (that is `asc`'s non-null signature); `maybe` is only
    /// preferred once the argument *is* known to be nullable.
    fn value_score(&self, cand: &Ty, arg: &Ty) -> i32 {
        let cand = self.resolve(cand);
        let arg = self.resolve(arg);
        let cand_maybe = matches!(cand, Ty::Con("maybe", _));
        let arg_maybe = matches!(arg, Ty::Con("maybe", _));
        let cand_var = matches!(cand, Ty::Var(_));
        let arg_var = matches!(arg, Ty::Var(_));
        match (cand_maybe, arg_maybe, cand_var, arg_var) {
            // Both open: the candidate that constrains nothing wins.
            (_, _, true, true) => 1,
            // A variable candidate still adapts to a known argument.
            (_, true, true, _) => 0,
            (_, false, true, _) => 2,
            // Nothing known about the argument: keep the non-null default.
            (true, _, false, true) => -1,
            (false, _, false, true) => 3,
            (c, a, _, _) if c == a => 3,
            _ => -3,
        }
    }

    pub(crate) fn fingerprint(&self, t: &Ty) -> String {
        let mut out = String::new();
        self.fingerprint_into(t, &mut out);
        out
    }

    pub(crate) fn fingerprint_into(&self, t: &Ty, out: &mut String) {
        match self.resolve(t) {
            Ty::Sym(s) => {
                out.push_str("sym(");
                out.push_str(&format!("{s:?}"));
                out.push(')');
            }
            Ty::Var(v) => {
                out.push('v');
                out.push_str(&v.to_string());
                out.push(':');
                out.push_str(&self.vars[v as usize].version.to_string());
            }
            Ty::Rigid(v, _) => {
                out.push('r');
                out.push_str(&v.to_string());
            }
            Ty::Gen(v) => {
                out.push('g');
                out.push_str(&v.to_string());
            }
            Ty::KeyAffix(n, s) => {
                out.push_str(n);
                out.push_str(&s);
                out.push('\'');
            }
            Ty::Con(n, args) => {
                out.push_str(n);
                out.push('(');
                for a in args {
                    self.fingerprint_into(&a, out);
                    out.push(',');
                }
                out.push(')');
            }
            Ty::Fun(a, b) => {
                out.push_str("fun(");
                self.fingerprint_into(&a, out);
                self.fingerprint_into(&b, out);
                out.push(')');
            }
            Ty::Row(fs, tail) => {
                out.push_str("row(");
                for (name, ty) in fs {
                    out.push_str(&name);
                    out.push('=');
                    self.fingerprint_into(&ty, out);
                    out.push(',');
                }
                self.fingerprint_into(&tail, out);
                out.push(')');
            }
            Ty::MapKey(m, r) => {
                out.push_str("keyMap(");
                out.push_str(&format!("{:?}", m));
                out.push(',');
                self.fingerprint_into(&r, out);
                out.push(')');
            }
            Ty::RemoveKey(k, row) => {
                out.push_str("removeKey(");
                self.fingerprint_into(&k, out);
                out.push(',');
                self.fingerprint_into(&row, out);
                out.push(')');
            }
            Ty::Merge(left, right) => {
                out.push_str("merge(");
                self.fingerprint_into(&left, out);
                out.push(',');
                self.fingerprint_into(&right, out);
                out.push(')');
            }
            Ty::MapValue(v, r) => {
                out.push_str("mapvalue(");
                out.push_str(&format!("{:?}", v));
                out.push(',');
                self.fingerprint_into(&r, out);
                out.push(')');
            }
            Ty::Empty => out.push_str("empty"),
        }
    }

    /// Type of a use of an overload set: the candidates' common shape.
    pub(crate) fn overload_type(
        &mut self,
        name: &str,
        m: usize,
        cands: &[usize],
        site: u32,
        sp: Span,
    ) -> Ty {
        let tys: Vec<Ty> = cands.iter().map(|&i| self.cand_scheme(m, i).ty).collect();
        let target = self.skeleton(&tys, &mut HashMap::new());
        let c = Cons::Overload {
            name: name.to_string(),
            module: m,
            cands: cands.to_vec(),
            target: target.clone(),
            origin: Origin::Site(site),
        };
        self.pending.push((c, sp));
        target
    }

    /// Anti-unification: shared structure is kept; positions where the
    /// candidates differ become variables (one per distinct combination).
    pub(crate) fn skeleton(&mut self, ts: &[Ty], memo: &mut HashMap<String, Ty>) -> Ty {
        let ts: Vec<Ty> = ts.iter().map(|t| self.resolve(t)).collect();
        match &ts[0] {
            Ty::Con(n, args)
                if ts
                    .iter()
                    .all(|t| matches!(t, Ty::Con(m, a) if m == n && a.len() == args.len())) =>
            {
                let (n, arity) = (*n, args.len());
                let args = (0..arity)
                    .map(|k| {
                        let col: Vec<Ty> = ts
                            .iter()
                            .map(|t| match t {
                                Ty::Con(_, a) => a[k].clone(),
                                _ => unreachable!(),
                            })
                            .collect();
                        self.skeleton(&col, memo)
                    })
                    .collect();
                Ty::Con(n, args)
            }
            Ty::Fun(..) if ts.iter().all(|t| matches!(t, Ty::Fun(..))) => {
                let (mut xs, mut ys) = (Vec::new(), Vec::new());
                for t in &ts {
                    if let Ty::Fun(a, b) = t {
                        xs.push((**a).clone());
                        ys.push((**b).clone());
                    }
                }
                fun(self.skeleton(&xs, memo), self.skeleton(&ys, memo))
            }
            _ => {
                let key = format!("{ts:?}");
                if let Some(t) = memo.get(&key) {
                    return t.clone();
                }
                let v = self.fresh();
                memo.insert(key, v.clone());
                v
            }
        }
    }

    pub(crate) fn record(&mut self, key: (usize, usize), site: u32, k: usize, c: Choice) {
        self.choices.entry(key).or_default().insert((site, k), c);
    }
}

/// The first concrete disagreement between a candidate and a use.
///
/// Only *concrete* types can disagree: a variable on either side means the
/// checker could not tell, and a message that guessed would be worse than none.
/// `depth` counts parameters, so the message can say "argument 2" rather than
/// printing two whole types.
fn differ(ck: &Checker<'_>, cand: &Ty, target: &Ty, depth: usize) -> Option<String> {
    let (c, t) = (ck.resolve(cand), ck.resolve(target));
    match (&c, &t) {
        (Ty::Fun(cp, c_rest), Ty::Fun(tp, t_rest)) => {
            differ(ck, cp, tp, depth + 1).or_else(|| differ(ck, c_rest, t_rest, depth))
        }
        (Ty::Con(nc, ac), Ty::Con(nt, at)) if nc == nt && ac.len() == at.len() => {
            for (a, b) in ac.iter().zip(at.iter()) {
                if let Some(m) = differ(ck, a, b, depth) {
                    return Some(m);
                }
            }
            None
        }
        (Ty::Con(_, _), Ty::Con(_, _)) => Some(where_at(depth, &c, &t, ck)),
        (Ty::Row(..), Ty::Row(..)) => None,
        _ => None,
    }
}

/// "argument 2 expects `int`, the use has `string`", or the same about the
/// result when the disagreement is past the last parameter.
fn where_at(depth: usize, expected: &Ty, found: &Ty, ck: &Checker<'_>) -> String {
    let expected = ck.show(expected);
    let found = ck.show(found);
    if depth == 0 {
        format!("the result expects `{expected}`, the use has `{found}`")
    } else {
        format!("argument {depth} expects `{expected}`, the use has `{found}`")
    }
}
