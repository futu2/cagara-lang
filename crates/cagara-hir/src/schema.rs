//! Output-schema computation and static validation of the relational IR:
//! column existence, key-mapper validity, join sides, and phase placement.

use crate::ir::{Expr, Loc, Rel, Side};
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
        Rel::Update(r, fs) => merge(fs, &schema(r)?),
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

/// Output columns of `update`: the input's, with each listed name replacing
/// the one it matches, and names the input does not have appended in order.
/// The input's positions are kept, so `update` reads as an overwrite.
pub fn merge_columns<T, U>(
    input: &[(String, T)],
    fs: &[(String, U)],
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = input.iter().map(|(n, _)| n.clone()).collect();
    if fs.is_empty() {
        return Err("`update` needs at least one field".into());
    }
    for (n, _) in fs {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    Ok(out)
}

fn merge(fs: &[(String, Expr)], cols: &[String]) -> Result<Vec<String>, String> {
    if fs.is_empty() {
        return Err("`update` needs at least one field".into());
    }
    let mut seen = Vec::new();
    for (n, e) in fs {
        if seen.contains(n) {
            return Err(format!("field `{n}` appears twice in `update`"));
        }
        seen.push(n.clone());
        refs(e, cols, "update").map_err(|m| format!("field `{n}`: {m}"))?;
        let phase = e.phase().map_err(|m| format!("field `{n}`: {m}"))?;
        rules::place(Place::Select, phase).map_err(|m| format!("field `{n}` {m}"))?;
    }
    let named = cols.iter().map(|c| (c.clone(), ())).collect::<Vec<_>>();
    merge_columns(&named, fs)
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
