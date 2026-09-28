//! Output-schema computation and static validation of the relational IR:
//! column existence, key-mapper validity, join sides, and phase placement.

use crate::ir::{Expr, KeyMapper, Phase, Rel, Side};

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
            match e.phase()? {
                Phase::Agg => Err("`where` cannot filter on an aggregate; filter the output of an `agg` stage instead".into()),
                Phase::Win => Err("`where` cannot filter on a window function; `select` it first, then filter the new column".into()),
                _ => Ok(c),
            }
        }
        Rel::Select(r, fs) => projection(fs, &schema(r)?, false),
        Rel::Agg(r, fs) => projection(fs, &schema(r)?, true),
        Rel::Order(r, ks) => {
            let c = schema(r)?;
            for (k, _) in ks {
                refs(k, &c, "order")?;
                if matches!(k.phase()?, Phase::Agg | Phase::Win) {
                    return Err("sort keys must be plain column expressions; compute aggregates or windows in an earlier stage".into());
                }
            }
            Ok(c)
        }
        Rel::Limit(r, _) | Rel::Offset(r, _) => schema(r),
        Rel::KeyMap(r, m) => Ok(m.apply(&schema(r)?)?.into_iter().map(|(_, new)| new).collect()),
        Rel::Join { left, right, on, .. } => {
            let (lc, rc) = (schema(left)?, schema(right)?);
            for (side, n) in on.columns() {
                let (cols, what) = match side {
                    Side::Left => (&lc, "left"),
                    Side::Right => (&rc, "right"),
                    Side::Single => {
                        return Err(format!(
                            "join predicates must say which input a column comes from: `.<{n}` (left) or `.>{n}` (right)"
                        ))
                    }
                };
                if !cols.contains(&n) {
                    return Err(format!("the {what} join input has no column `{n}`; available: {}", cols.join(", ")));
                }
            }
            if matches!(on.phase()?, Phase::Agg | Phase::Win) {
                return Err("join predicates cannot contain aggregates or window functions".into());
            }
            let mut out = lc.clone();
            out.extend(rc.into_iter().filter(|n| !lc.contains(n)));
            Ok(out)
        }
    }
}

fn projection(fs: &[(String, Expr)], cols: &[String], agg: bool) -> Result<Vec<String>, String> {
    let stage = if agg { "agg" } else { "select" };
    if fs.is_empty() {
        return Err(format!("`{stage}` needs at least one field"));
    }
    for (n, e) in fs {
        refs(e, cols, stage).map_err(|m| format!("field `{n}`: {m}"))?;
        let phase = e.phase().map_err(|m| format!("field `{n}`: {m}"))?;
        match (agg, phase) {
            (false, Phase::Agg) => {
                return Err(format!("field `{n}` is an aggregate; aggregates belong in `agg`, not `select`"))
            }
            (true, Phase::Row) => {
                return Err(format!("field `{n}` uses a column that is not grouped; wrap it in `group` or aggregate it"))
            }
            (true, Phase::Win) => {
                return Err(format!("field `{n}` is a window function; use it in a `select` stage after `agg`"))
            }
            _ => {}
        }
    }
    Ok(fs.iter().map(|(n, _)| n.clone()).collect())
}

fn refs(e: &Expr, cols: &[String], ctx: &str) -> Result<(), String> {
    for (side, n) in e.columns() {
        match side {
            Side::Single if !cols.contains(&n) => {
                return Err(format!("no column `{n}` in the input of `{ctx}`; available: {}", cols.join(", ")))
            }
            Side::Left | Side::Right => {
                return Err(format!("`.<{n}` / `.>{n}` can only be used in a join predicate"))
            }
            _ => {}
        }
    }
    Ok(())
}

impl KeyMapper {
    /// Map input columns to `(old, new)` pairs in output order, rejecting
    /// missing sources, duplicate selectors, and output collisions.
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
                        let new = ps.iter().find(|p| &p.0 == c).map_or_else(|| c.clone(), |p| p.1.clone());
                        (c.clone(), new)
                    })
                    .collect()
            }
            KeyMapper::Prefix(p) => cols.iter().map(|c| (c.clone(), format!("{p}{c}"))).collect(),
            KeyMapper::Suffix(s) => cols.iter().map(|c| (c.clone(), format!("{c}{s}"))).collect(),
        };
        let names: Vec<String> = out.iter().map(|p| p.1.clone()).collect();
        match first_dup(&names) {
            Some(d) => Err(format!("key mapping would produce column `{d}` twice")),
            None => Ok(out),
        }
    }
}

fn first_dup(xs: &[String]) -> Option<&String> {
    xs.iter().enumerate().find(|(i, x)| xs[..*i].contains(x)).map(|(_, x)| x)
}

fn distinct(ks: &[String]) -> Result<(), String> {
    match first_dup(ks) {
        Some(d) => Err(format!("column `{d}` is listed twice")),
        None => Ok(()),
    }
}
