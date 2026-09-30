//! Output-schema computation and static validation of the relational IR:
//! column existence, key-mapper validity, join sides, and phase placement.

use crate::ir::{Expr, KeyMapper, Loc, Rel, Side};
use crate::rules::{self, Place};

pub fn schema(rel: &Rel) -> Result<Vec<String>, String> {
    match rel {
        Rel::Table { columns: Some(c), .. } => Ok(c.clone()),
        Rel::Table { schema, name, columns: None } => Err(format!(
            "the columns of table `{schema}.{name}` are unknown; give its definition a closed type, \
             e.g. `t : query {{ id = int }} = table \"{schema}\" \"{name}\"`"
        )),
        Rel::Where(r, e) => {
            let c = schema(r)?;
            refs(e, &c, "where")?;
            rules::place(Place::Where, e.phase()?)?;
            Ok(c)
        }
        Rel::Select(r, fs) => projection(fs, &schema(r)?, false),
        Rel::Agg(r, fs) => projection(fs, &schema(r)?, true),
        Rel::Order(r, ks) => {
            let c = schema(r)?;
            for (k, _) in ks {
                refs(k, &c, "order")?;
                rules::place(Place::Key, k.phase()?)?;
            }
            Ok(c)
        }
        Rel::Limit(r, _) | Rel::Offset(r, _) | Rel::Distinct(r) | Rel::At(_, r) => schema(r),
        Rel::KeyMap(r, m) => Ok(m.apply(&schema(r)?)?.into_iter().map(|(_, new)| new).collect()),
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => {
            let (lc, rc) = (schema(left)?, schema(right)?);
            for (side, n) in on.columns() {
                let (cols, what) = match side {
                    Side::Left => (&lc, "left"),
                    Side::Right => (&rc, "right"),
                    Side::Single => return Err(rules::needs_side(&n)),
                };
                if !cols.contains(&n) {
                    return Err(format!("the {what} join input has no column `{n}`; available: {}", cols.join(", ")));
                }
            }
            rules::place(Place::JoinOn, on.phase()?)?;
            if matches!(kind, crate::ir::JoinKind::Semi | crate::ir::JoinKind::Anti) {
                return Ok(lc);
            }
            let named = |cs: Vec<String>| cs.into_iter().map(|c| (c, ())).collect::<Vec<_>>();
            let out = rules::join_columns(&named(lc), &named(rc));
            Ok(out.into_iter().map(|(c, ())| c).collect())
        }
        Rel::Set { left, right, .. } => {
            let lc = schema(left)?;
            let rc = schema(right)?;
            if lc != rc {
                return Err(format!(
                    "set-operation inputs must have the same columns; left has [{}], right has [{}]",
                    lc.join(", "),
                    rc.join(", ")
                ));
            }
            Ok(lc)
        }
    }
}

/// Like [`schema`], but an error carries the location of the innermost
/// failing stage (the nearest enclosing `Rel::At`).
pub fn schema_located(rel: &Rel) -> Result<Vec<String>, (Option<Loc>, String)> {
    schema(rel).map_err(|msg| blame(rel, None, msg))
}

fn blame(rel: &Rel, loc: Option<Loc>, msg: String) -> (Option<Loc>, String) {
    let loc = match rel {
        Rel::At(l, _) => Some(*l),
        _ => loc,
    };
    for c in rel.children() {
        if let Err(m) = schema(c) {
            return blame(c, loc, m);
        }
    }
    (loc, msg)
}

fn projection(fs: &[(String, Expr)], cols: &[String], agg: bool) -> Result<Vec<String>, String> {
    let stage = if agg { "agg" } else { "select" };
    if fs.is_empty() {
        return Err(format!("`{stage}` needs at least one field"));
    }
    for (n, e) in fs {
        refs(e, cols, stage).map_err(|m| format!("field `{n}`: {m}"))?;
        let phase = e.phase().map_err(|m| format!("field `{n}`: {m}"))?;
        let at = if agg { Place::Agg } else { Place::Select };
        rules::place(at, phase).map_err(|m| format!("field `{n}` {m}"))?;
    }
    Ok(fs.iter().map(|(n, _)| n.clone()).collect())
}

fn refs(e: &Expr, cols: &[String], ctx: &str) -> Result<(), String> {
    for (side, n) in e.columns() {
        match side {
            Side::Single if !cols.contains(&n) => {
                return Err(format!(
                    "no column `{n}` in the input of `{ctx}`; available: {}",
                    cols.join(", ")
                ))
            }
            Side::Left | Side::Right => return Err(rules::JOIN_ONLY.into()),
            _ => {}
        }
    }
    Ok(())
}

impl KeyMapper {
    /// Map input columns to `(old, new)` pairs in output order, rejecting
    /// missing sources, duplicate selectors, output collisions, and an empty
    /// output. The checker and the validator both use this.
    pub fn apply(&self, cols: &[String]) -> Result<Vec<(String, String)>, String> {
        let exists = |k: &String| {
            if cols.contains(k) {
                Ok(())
            } else {
                Err(format!("no column `{k}`; available: {}", cols.join(", ")))
            }
        };
        let same = |c: &String| (c.clone(), c.clone());
        let out: Vec<(String, String)> = match self {
            KeyMapper::Only(ks) => {
                distinct(ks)?;
                ks.iter().try_for_each(exists)?;
                ks.iter().map(same).collect()
            }
            KeyMapper::Drop(ks) => {
                distinct(ks)?;
                ks.iter().try_for_each(exists)?;
                cols.iter().filter(|c| !ks.contains(c)).map(same).collect()
            }
            KeyMapper::Replace(ps) => {
                let srcs: Vec<String> = ps.iter().map(|p| p.0.clone()).collect();
                distinct(&srcs)?;
                srcs.iter().try_for_each(exists)?;
                cols.iter()
                    .map(|c| {
                        let new = ps
                            .iter()
                            .find(|p| &p.0 == c)
                            .map_or_else(|| c.clone(), |p| p.1.clone());
                        (c.clone(), new)
                    })
                    .collect()
            }
            KeyMapper::Prefix(p) => cols
                .iter()
                .map(|c| (c.clone(), format!("{p}{c}")))
                .collect(),
            KeyMapper::Suffix(s) => cols
                .iter()
                .map(|c| (c.clone(), format!("{c}{s}")))
                .collect(),
        };
        if out.is_empty() {
            return Err("key mapping leaves no columns".into());
        }
        let names: Vec<String> = out.iter().map(|p| p.1.clone()).collect();
        match first_dup(&names) {
            Some(d) => Err(format!("key mapping would produce column `{d}` twice")),
            None => Ok(out),
        }
    }
}

fn first_dup(xs: &[String]) -> Option<&String> {
    xs.iter()
        .enumerate()
        .find(|(i, x)| xs[..*i].contains(x))
        .map(|(_, x)| x)
}

fn distinct(ks: &[String]) -> Result<(), String> {
    match first_dup(ks) {
        Some(d) => Err(format!("column `{d}` is listed twice")),
        None => Ok(()),
    }
}
