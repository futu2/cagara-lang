//! Output-schema computation and static validation of the relational IR:
//! column existence, key-map validity, join sides, and phase placement.

use crate::ir::{Expr, Loc, Rel, Side};
use crate::rules::{self, Place};
use std::collections::HashMap;

pub fn schema(rel: &Rel) -> Result<Vec<String>, String> {
    // An explicit post-order walk with a memo, not recursion. The IR is a
    // chain as deep as the program's pipeline (one node per stage), so a
    // recursive walk used one stack frame per stage; combined with a caller
    // that was already deep in the same tree (the SQL lowerer asks for a
    // subtree's columns while lowering it) a 50-stage pipeline overflowed the
    // stack and aborted the process. The memo also makes the walk linear
    // rather than re-deriving shared subtrees.
    enum Step<'a> {
        Visit(&'a Rel),
        Finish(&'a Rel),
    }
    let mut done: HashMap<usize, Result<Vec<String>, String>> = HashMap::new();
    let mut stack = vec![Step::Visit(rel)];
    while let Some(step) = stack.pop() {
        let r = match step {
            Step::Visit(r) => {
                let addr = r as *const Rel as usize;
                if done.contains_key(&addr) {
                    continue;
                }
                if let Some(c) = child_of(r) {
                    stack.push(Step::Finish(r));
                    stack.push(Step::Visit(c));
                    // A join or set operation has two inputs.
                    if let Some(c2) = second_child_of(r) {
                        stack.push(Step::Visit(c2));
                    }
                } else {
                    // A leaf: its schema needs no other node.
                    stack.push(Step::Finish(r));
                }
                continue;
            }
            Step::Finish(r) => r,
        };
        // A node whose inputs failed fails the same way; recording it (rather
        // than returning here) keeps the walk and the memo consistent, and the
        // first error in post-order is the innermost one.
        let cols = finish_schema(r, &done);
        done.insert(r as *const Rel as usize, cols);
    }
    done.remove(&(rel as *const Rel as usize))
        .expect("the root was visited")
}

/// The single input of a unary relation. A join or set operation has two, and
/// is handled by [`second_child_of`]; a table has none.
fn child_of(rel: &Rel) -> Option<&Rel> {
    match rel {
        Rel::Table { .. } => None,
        Rel::Where(r, _)
        | Rel::Select(r, _)
        | Rel::Update(r, _)
        | Rel::Omit(r, _)
        | Rel::Prefix(r, _)
        | Rel::Suffix(r, _)
        | Rel::Agg(r, _)
        | Rel::Order(r, _)
        | Rel::Limit(r, _)
        | Rel::Offset(r, _)
        | Rel::Distinct(r)
        | Rel::At(_, r) => Some(r),
        Rel::Join { left, .. } => Some(left),
        Rel::Set { left, .. } => Some(left),
    }
}

/// The second input of a join or set operation, if it has one.
fn second_child_of(rel: &Rel) -> Option<&Rel> {
    match rel {
        Rel::Join { right, .. } => Some(right),
        Rel::Set { right, .. } => Some(right),
        _ => None,
    }
}

/// The schema of one node, given that every input already has one. This is the
/// body of the old recursive `schema`, with the recursive calls replaced by
/// memo lookups.
fn finish_schema(
    rel: &Rel,
    done: &HashMap<usize, Result<Vec<String>, String>>,
) -> Result<Vec<String>, String> {
    let of = |r: &Rel| -> Result<Vec<String>, String> {
        done.get(&(r as *const Rel as usize))
            .cloned()
            .unwrap_or_else(|| Err("internal: input schema was not computed".to_string()))
    };
    match rel {
        Rel::Table { columns: Some(c), .. } => Ok(c.clone()),
        Rel::Table {
            schema,
            name,
            columns: None,
        } => Err(format!(
            "the columns of table `{schema}.{name}` are unknown; give its definition a closed type, \
             e.g. `t : query {{ id = int }} = table \"{schema}\" \"{name}\"`"
        )),
        Rel::Where(r, e) => {
            let c = of(r)?;
            refs(e, &c, "where")?;
            rules::place(Place::Where, e.phase()?)?;
            Ok(c)
        }
        Rel::Select(r, fs) => projection(fs, &of(r)?, false),
        Rel::Update(r, fs) => merge(fs, &of(r)?),
        Rel::Omit(r, k) => omit_columns(&of(r)?, k),
        // A rename preserves the columns' positions and types; only the labels
        // change. The checker computed the same thing with `keyMap (prefix s)`.
        Rel::Prefix(r, affix) => Ok(rename_columns(&of(r)?, affix, true)),
        Rel::Suffix(r, affix) => Ok(rename_columns(&of(r)?, affix, false)),
        Rel::Agg(r, fs) => projection(fs, &of(r)?, true),
        Rel::Order(r, ks) => {
            let c = of(r)?;
            for (k, _) in ks {
                refs(k, &c, "order")?;
                rules::place(Place::Key, k.phase()?)?;
            }
            Ok(c)
        }
        Rel::Limit(r, _) | Rel::Offset(r, _) | Rel::Distinct(r) | Rel::At(_, r) => of(r),
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => {
            let (lc, rc) = (of(left)?, of(right)?);
            for (side, n) in on.columns() {
                let (cols, what) = match side {
                    Side::Left => (&lc, "left"),
                    Side::Right => (&rc, "right"),
                    Side::Single => return Err(rules::needs_side(&n)),
                };
                if !cols.contains(&n) {
                    return Err(format!(
                        "the {what} join input has no column `{n}`; available: {}",
                        cols.join(", ")
                    ));
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
            let lc = of(left)?;
            let rc = of(right)?;
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

/// Columns of `update`. The checker and the evaluator both reject an empty
/// field list and a repeated field before a `Rel` is ever built, so the only
/// checks left here are the ones they do not do: every field's value refers
/// to an input column and sits in the select phase. The empty case is still
/// caught, by `merge_columns`.
fn merge(fs: &[(String, Expr)], cols: &[String]) -> Result<Vec<String>, String> {
    for (n, e) in fs {
        refs(e, cols, "update").map_err(|m| format!("field `{n}`: {m}"))?;
        let phase = e.phase().map_err(|m| format!("field `{n}`: {m}"))?;
        rules::place(Place::Select, phase).map_err(|m| format!("field `{n}` {m}"))?;
    }
    let named = cols.iter().map(|c| (c.clone(), ())).collect::<Vec<_>>();
    merge_columns(&named, fs)
}

/// Columns of `omit "k"`: the input's, minus `k`, in the input's order. The
/// checker reaches the same columns through the row equation
/// `input ~ { k : t | output }`; this is the validator's side of that fact.
pub fn omit_columns(cols: &[String], key: &str) -> Result<Vec<String>, String> {
    if !cols.iter().any(|c| c == key) {
        return Err(format!("no column `{key}`; available: {}", cols.join(", ")));
    }
    Ok(cols.iter().filter(|c| *c != key).cloned().collect())
}

/// Columns of `prefix "s"` / `suffix "s"`: every input name gains `affix` in
/// front or at the end.
///
/// Unlike `omit`, a rename cannot fail on the columns themselves — the affix is
/// a literal, and `name ↦ affix <> name` is injective for a fixed affix, so
/// distinct names stay distinct and no name can become empty. The only way a
/// rename goes wrong is a clash with something outside the row, which is not
/// this function's business.
pub fn rename_columns(cols: &[String], affix: &str, prefix: bool) -> Vec<String> {
    cols.iter()
        .map(|c| {
            if prefix {
                format!("{affix}{c}")
            } else {
                format!("{c}{affix}")
            }
        })
        .collect()
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
