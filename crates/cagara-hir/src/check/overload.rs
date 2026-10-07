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
        let defs = self.env.defs;
        let ann = if m == self.module {
            defs[i].ty.as_ref().map(|t| self.annotation(t))
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

    pub(crate) fn fingerprint(&self, t: &Ty) -> String {
        let mut out = String::new();
        self.fingerprint_into(t, &mut out);
        out
    }

    pub(crate) fn fingerprint_into(&self, t: &Ty, out: &mut String) {
        match self.resolve(t) {
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
