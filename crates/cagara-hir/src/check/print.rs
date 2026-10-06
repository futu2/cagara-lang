//! [`Checker`] type printing, used by diagnostics and hover.
//!
//! Split out of the single `check.rs` for readability only: whole items and
//! whole `Checker` methods were moved, and no logic was changed.

use super::*;

impl<'w> Checker<'w> {
    // ── printing ───────────────────────────────────────────────────────────

    pub(crate) fn show(&self, t: &Ty) -> String {
        let t = self.zonk(t);
        Printer::default().ty(&t, 0)
    }

    pub(crate) fn show_scheme(&self, s: &Scheme) -> String {
        let mut p = Printer {
            gens: s.gens.iter().map(|g| g.name.clone()).collect(),
            ..Printer::default()
        };
        p.ty(&s.ty, 0)
    }
}

#[derive(Default)]
struct Printer {
    names: HashMap<u32, String>,
    /// Signature names of a scheme's `Gen` variables; others get fresh names.
    gens: Vec<Option<String>>,
    gen_names: HashMap<u32, String>,
}

impl Printer {
    pub(crate) fn var(&mut self, v: u32) -> String {
        let n = self.names.len() + self.gen_names.len();
        self.names.entry(v).or_insert_with(|| var_name(n)).clone()
    }

    /// `prec`: 0 = top, 1 = function argument, 2 = constructor argument.
    pub(crate) fn ty(&mut self, t: &Ty, prec: u8) -> String {
        let paren = |s: String, need: bool| if need { format!("({s})") } else { s };
        match t {
            Ty::Var(v) => self.var(*v),
            Ty::Rigid(_, n) => n.clone(),
            Ty::Gen(k) => match self.gens.get(*k as usize).cloned().flatten() {
                Some(n) => n,
                None => {
                    let n = self.names.len() + self.gen_names.len();
                    self.gen_names
                        .entry(*k)
                        .or_insert_with(|| var_name(n))
                        .clone()
                }
            },
            // Omit/value-wrapper markers are string positions. Directional
            // affixes retain their mapper witness in the printed type so the
            // row transformation is visible and cannot look unrelated.
            Ty::Con("key_omit" | "key_pattern" | "key_replacement" | "value_wrapper", _) => {
                "string".into()
            }
            Ty::Con(n @ ("prefixAffix" | "suffixAffix"), args) if args.len() == 1 => {
                format!("{n} {}", self.ty(&args[0], 2))
            }
            Ty::Empty => "{}".into(),
            Ty::Row(fs, tail) => {
                let body: Vec<String> = fs
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", self.ty(v, 0)))
                    .collect();
                match &**tail {
                    Ty::Empty => format!("{{ {} }}", body.join(", ")),
                    t => format!("{{ {} | {} }}", body.join(", "), self.ty(t, 0)),
                }
            }
            Ty::MapKey(km, row) => {
                // Print the term as written. The *inner* row is already the
                // renamed row once it has been stored, so applying the mapper
                // again here would double-apply it (`u_u_id`). Reducing is the
                // store's job, not the printer's.
                format!("keyMap({}, {})", self.ty(km, 2), self.ty(row, 2))
            }
            Ty::Merge(left, right) => {
                format!("merge({}, {})", self.ty(left, 2), self.ty(right, 2))
            }
            Ty::MapValue(vm, row) => {
                format!("mapValue({:?}, {})", vm, self.ty(row, 2))
            }
            Ty::Fun(a, b) => {
                let s = format!("{} -> {}", self.ty(a, 1), self.ty(b, 0));
                paren(s, prec >= 1)
            }
            Ty::Con("expr", a) => {
                let inner = format!("expr {} {}", self.ty(&a[1], 2), self.ty(&a[2], 2));
                match &a[0] {
                    Ty::Con(p @ ("agg" | "win"), _) => paren(format!("{p} ({inner})"), prec >= 2),
                    _ => paren(inner, prec >= 2),
                }
            }
            Ty::Con("record_expr", a) if a.len() == 4 => {
                let inner = format!("expr {} (row {})", self.ty(&a[1], 2), self.ty(&a[3], 2));
                match &a[0] {
                    Ty::Con(p @ ("agg" | "win"), _) => paren(format!("{p} ({inner})"), prec >= 2),
                    _ => paren(inner, prec >= 2),
                }
            }
            // A key affix prints as its constructor applied to the literal, so
            // `keyMap (prefix "u_") r` reads back the way it was written.
            Ty::KeyAffix(n, s) => {
                let short = match n.strip_prefix("KeyMap").unwrap_or(n) {
                    "Prefix" => "prefix",
                    "Suffix" => "suffix",
                    other => other,
                };
                paren(format!("{short} {s:?}"), prec >= 2)
            }
            // A zero-argument marker is kept for compatibility with older
            // schemes; new directional signatures always carry one witness.
            Ty::Con("prefixAffix" | "suffixAffix", _) => "string".into(),
            Ty::Con("KeyMapId", args) if args.is_empty() => "id".into(),
            Ty::Con("KeyMapCompose", args) if args.len() == 2 => {
                let f = self.ty(&args[0], 2);
                let g = self.ty(&args[1], 2);
                paren(format!("compose {f} {g}"), prec >= 2)
            }
            Ty::Con(n, args) if args.is_empty() => n.to_string(),
            Ty::Con(n, args) => {
                let args: Vec<String> = args.iter().map(|a| self.ty(a, 2)).collect();
                paren(format!("{n} {}", args.join(" ")), prec >= 2)
            }
        }
    }
}

fn var_name(n: usize) -> String {
    if n < 26 {
        ((b'a' + n as u8) as char).to_string()
    } else {
        format!("t{n}")
    }
}
