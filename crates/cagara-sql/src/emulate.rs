//! Dialect-directed `Rel -> Rel` rewrites: the constructs a target cannot say,
//! written in SQL it can.
//!
//! [`crate::rewrite`] is the dialect-free pass — rules that are true everywhere
//! and remove a derived table. This is its counterpart: rules that exist *because
//! of the target*. When [`crate::dialect::support`] answers
//! [`Emulated`](crate::dialect::Support::Emulated) for a construct, the query is
//! rewritten here into equivalent relational algebra before the lowerer sees it,
//! so the target is handed SQL it has, and the language keeps the construct.
//!
//! # Why the rewrite is here and not in the SQL text
//!
//! The alternative — emitting the construct and post-processing the string — was
//! how `fullJoin` and grouping sets used to be handled: they were *refused*, with
//! a message telling the reader to write the union themselves. That put the work
//! on every reader, and it made the language's surface depend on the target,
//! which is the thing this backend is supposed to stop doing.
//!
//! Doing it in the IR means the lowerer, the schema validator, and the dialect
//! passes all see ordinary nodes, and there is no second implementation of
//! `SELECT` assembly to keep in step with the first.
//!
//! # What is emulated
//!
//! | construct | target | rewritten to |
//! |---|---|---|
//! | `fullJoin` | the MySQL family | `LEFT JOIN` `UNION ALL` the right rows with no match |
//! | `rollup` / `cube` | SQLite, the MySQL family | one `agg` per grouping set, `UNION ALL`ed |
//!
//! Both rewrites produce *exactly* the rows the construct means, not an
//! approximation: the full join's second branch is an anti-join on the same
//! predicate (so a matched row is never duplicated, whatever its columns hold),
//! and the grouping sets are enumerated rather than approximated by
//! `WITH ROLLUP`.

use crate::dialect::{support, Cap, Support};
use cagara_hir::ir::{Expr, GroupingShape, JoinKind, Rel, SetKind, Side, WinSpec};
use sqlglot_rust::Dialect;

/// Rewrite every construct `d` lacks, bottom-up.
///
/// Bottom-up because a branch produced by a rule can itself contain a construct
/// the target lacks — an emulated full join inside an aggregate whose keys are
/// rolled up — and the inner one has to be gone before the outer rewrite decides
/// what to union.
pub(crate) fn emulate(rel: &Rel, d: Dialect) -> Rel {
    let mut r = rel.clone();
    for child in r.children_mut() {
        *child = emulate(child, d);
    }
    match step(&r, d) {
        Some(next) => next,
        None => r,
    }
}

/// One node against the capability table.
///
/// The `At` wrapper is stripped for the decision and put back on the result: it
/// carries the span a diagnostic points at, and an emulated node still came from
/// the stage the reader wrote.
fn step(r: &Rel, d: Dialect) -> Option<Rel> {
    let loc = match r {
        Rel::At(loc, _) => Some(*loc),
        _ => None,
    };
    let body = match r.bare() {
        Rel::Join {
            kind: JoinKind::Full,
            left,
            right,
            on,
        } if support(Cap::FullJoin, d) == Support::Emulated => full_join(left, right, on),
        Rel::Agg(input, fs)
            if support(Cap::GroupingSets, d) == Support::Emulated
                && fs.iter().any(|(_, e)| e.any(&is_grouping)) =>
        {
            grouping_sets(input, fs)
        }
        _ => return None,
    };
    Some(match loc {
        Some(loc) => Rel::At(loc, Box::new(body)),
        None => body,
    })
}

fn is_grouping(e: &Expr) -> bool {
    matches!(e, Expr::Grouping(..))
}

/// `l FULL JOIN r ON p` as `(l LEFT JOIN r ON p) UNION ALL (r without a match)`.
///
/// The second branch is the right rows the left join did not keep, and the
/// honest way to name them is the predicate itself: a right row belongs there
/// exactly when no left row satisfies `p` against it. `NOT EXISTS` over the
/// left input says that, so it is an anti-join of `r` against `l` with the same
/// predicate and its sides swapped.
///
/// The obvious alternative — `RIGHT JOIN … WHERE <some left column> IS NULL` — is
/// wrong, and measurably so. It has to name a column to test, and it is only correct
/// if that column is non-NULL on *every* matched row, which no general predicate
/// promises. Measured on SQLite with `l(1, NULL)` against `r(1, 'x'), (2, …)` and
/// `ON l.id = r.uid`:
///
/// ```text
/// FULL JOIN                      -> 1|NULL|1|x ; NULL|NULL|2|unmatched
/// anti-join (what this emits)    -> 1|NULL|1|x ; NULL|NULL|2|unmatched
/// RIGHT JOIN … l.name IS NULL    -> 1|NULL|1|x ; 1|NULL|1|x ; NULL|NULL|2|unmatched
/// ```
///
/// The matched row appears twice in the last one, because it *is* matched and its
/// `name` is also NULL. The anti-join cannot double-count, because existence is not a
/// property of the row's values.
///
/// The anti-join's output is `r`'s columns, so it is projected into the full
/// join's row shape: `l`'s columns become the NULL the outer join promised, and
/// `r`'s columns are themselves.
fn full_join(left: &Rel, right: &Rel, on: &Expr) -> Rel {
    let lc = columns(left);
    let rc = columns(right);
    let joined = union_columns(&lc, &rc);
    let outer = Rel::Join {
        kind: JoinKind::Left,
        left: Box::new(left.clone()),
        right: Box::new(right.clone()),
        on: on.clone(),
    };
    let unmatched = Rel::Join {
        kind: JoinKind::Anti,
        left: Box::new(right.clone()),
        right: Box::new(left.clone()),
        on: swap_sides(on),
    };
    // A name in `lc` comes from the left input, which is absent in this branch;
    // a name that is only in `rc` is the right row's own column. A name in both
    // is the left's — `join_columns` keeps the left one — so it is NULL here too.
    let fields = joined
        .into_iter()
        .map(|n| {
            let e = if lc.contains(&n) {
                null()
            } else {
                Expr::Col(Side::Single, n.clone())
            };
            (n, e)
        })
        .collect();
    Rel::Set {
        kind: SetKind::UnionAll,
        left: Box::new(outer),
        right: Box::new(Rel::Select(Box::new(unmatched), fields)),
    }
}

/// `GROUP BY ROLLUP(a, b), CUBE(c)` as one `agg` per grouping set, `UNION ALL`ed.
///
/// A grouping set is a *list of keys to group by*; the clause names a family of
/// them. `ROLLUP(a, b)` is `{(a, b), (a), ()}` and `CUBE(a, b)` is every subset,
/// and two shapes in one clause are their cross product — so enumerating them is
/// the definition, not a heuristic.
///
/// Each set becomes an `agg` stage of its own over the same input, with the keys
/// it excludes written as NULL. That is what the clause means: a subtotal row is
/// a row where the rolled-up keys have no value. Writing NULL rather than dropping
/// the field is what keeps every branch the same shape, which is what makes them
/// union-able — and it is why the checker types a rolled-up key as `maybe`.
fn grouping_sets(input: &Rel, fs: &[(String, Expr)]) -> Rel {
    // The keys are discovered the way the lowerer discovers them, so the two
    // agree about which occurrences are keys and which are plain columns.
    let mut keys: Vec<(Option<GroupingShape>, &Expr)> = Vec::new();
    for (_, e) in fs {
        collect_groups(e, &mut keys);
    }
    let sets = grouping_set_list(&keys);
    let mut branches = sets.into_iter().map(|included| {
        let branch = fs
            .iter()
            .map(|(n, e)| (n.clone(), select_groups(e, &included)))
            .collect();
        Rel::Agg(Box::new(input.clone()), branch)
    });
    let first = branches
        .next()
        .expect("a grouping-set stage has at least one set");
    branches.fold(first, |left, right| Rel::Set {
        kind: SetKind::UnionAll,
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// Every grouping set the clause names, as the marked keys each one keeps.
///
/// The first set is the full key list, so the emitted `UNION ALL` starts with the
/// rows the plain `GROUP BY` would have produced; the coarser sets follow. That
/// order is not semantically required — `UNION ALL` has no row order — but it
/// makes the generated SQL read like the clause.
fn grouping_set_list<'a>(
    keys: &[(Option<GroupingShape>, &'a Expr)],
) -> Vec<Vec<(GroupingShape, &'a Expr)>> {
    // One entry per shape, in the order the shapes were written; `push_group` has
    // already deduplicated each (shape, key) pair.
    let mut shapes: Vec<(GroupingShape, Vec<&Expr>)> = Vec::new();
    for (shape, key) in keys {
        let Some(shape) = shape else { continue };
        match shapes.iter_mut().find(|(s, _)| s == shape) {
            Some((_, list)) => {
                if !list.contains(key) {
                    list.push(key);
                }
            }
            None => shapes.push((*shape, vec![*key])),
        }
    }
    // Each shape's own family of sets, as the indices of the keys it keeps.
    let families: Vec<Vec<Vec<usize>>> = shapes
        .iter()
        .map(|(shape, list)| {
            let n = list.len();
            match shape {
                // `ROLLUP(a, b)` is `{(a, b), (a), ()}`: every prefix, longest
                // first.
                GroupingShape::Rollup => (0..=n).rev().map(|keep| (0..keep).collect()).collect(),
                // `CUBE(a, b)` is every subset; the full one first.
                GroupingShape::Cube => (0..(1usize << n))
                    .rev()
                    .map(|mask| (0..n).filter(|i| mask & (1 << i) != 0).collect())
                    .collect(),
            }
        })
        .collect();
    // The clause's sets are the cross product of its shapes' families.
    let mut sets: Vec<Vec<(GroupingShape, &Expr)>> = vec![Vec::new()];
    for (shape_index, family) in families.iter().enumerate() {
        let (shape, list) = &shapes[shape_index];
        let mut next = Vec::with_capacity(sets.len() * family.len());
        for set in &sets {
            for keep in family {
                let mut with = set.clone();
                with.extend(keep.iter().map(|&i| (*shape, list[i])));
                next.push(with);
            }
        }
        sets = next;
    }
    sets
}

/// `e` with each grouping key it carries either kept as a plain key or replaced
/// by NULL.
///
/// A kept key becomes `group`, which is the same clause with the whole key list;
/// an excluded one becomes a NULL literal, which is the subtotal row's value. The
/// substitution happens inside the expression, not only at its root, because a key
/// can be used in arithmetic (`n2 = count * rollup .k` is unusual but legal).
fn select_groups(e: &Expr, included: &[(GroupingShape, &Expr)]) -> Expr {
    match e {
        Expr::Grouping(shape, key) => {
            if included
                .iter()
                .any(|(s, k)| s == shape && *k == key.as_ref())
            {
                Expr::Group(Box::new(select_groups(key, included)))
            } else {
                null()
            }
        }
        Expr::Group(key) => Expr::Group(Box::new(select_groups(key, included))),
        Expr::Col(..) | Expr::Lit(_) => e.clone(),
        Expr::Tpl(sql, args) => Expr::Tpl(
            sql.clone(),
            args.iter().map(|a| select_groups(a, included)).collect(),
        ),
        Expr::In(value, list, negated) => Expr::In(
            Box::new(select_groups(value, included)),
            list.iter().map(|a| select_groups(a, included)).collect(),
            *negated,
        ),
        Expr::Agg(sql, args, filter, order) => Expr::Agg(
            sql.clone(),
            args.iter().map(|a| select_groups(a, included)).collect(),
            filter
                .as_ref()
                .map(|p| Box::new(select_groups(p, included))),
            order
                .iter()
                .map(|(e, asc)| (select_groups(e, included), *asc))
                .collect(),
        ),
        Expr::Win(sql, args, spec) => Expr::Win(
            sql.clone(),
            args.iter().map(|a| select_groups(a, included)).collect(),
            Box::new(WinSpec {
                partition: spec
                    .partition
                    .iter()
                    .map(|a| select_groups(a, included))
                    .collect(),
                order: spec
                    .order
                    .iter()
                    .map(|(k, asc)| (select_groups(k, included), *asc))
                    .collect(),
                frame: spec.frame,
            }),
        ),
    }
}

/// The SQL NULL, as the IR spells it.
///
/// `nothing` lowers to `sql "NULL"`, so this is the same node the language's own
/// absent value produces — not a special case the lowerer has to learn.
fn null() -> Expr {
    Expr::Tpl("NULL".into(), vec![])
}

/// A relation's output columns, or none if they cannot be computed.
///
/// `schema` has already run by the time this pass does, so an unknown table
/// cannot reach here; an empty answer would still be a wrong rewrite, so the
/// caller treats "no columns" as "nothing to line up" rather than as an error.
fn columns(r: &Rel) -> Vec<String> {
    cagara_hir::schema::schema(r).unwrap_or_default()
}

/// The output columns of a join of two rows, in the order the lowerer emits them.
fn union_columns(left: &[String], right: &[String]) -> Vec<String> {
    let named = |cs: &[String]| cs.iter().map(|c| (c.clone(), ())).collect::<Vec<_>>();
    cagara_hir::rules::join_columns(&named(left), &named(right))
        .into_iter()
        .map(|(c, ())| c)
        .collect()
}

/// The same predicate with the two join sides exchanged.
///
/// An anti-join of `r` against `l` reads the predicate with `l` on the right, so
/// `.<id == .>user_id` becomes `.>id == .<user_id`. A `Single` column cannot
/// appear in a join predicate — the checker refuses it — so there is nothing to
/// do for one, and it is left as written.
fn swap_sides(e: &Expr) -> Expr {
    match e {
        Expr::Col(Side::Left, n) => Expr::Col(Side::Right, n.clone()),
        Expr::Col(Side::Right, n) => Expr::Col(Side::Left, n.clone()),
        Expr::Col(Side::Single, n) => Expr::Col(Side::Single, n.clone()),
        Expr::Lit(_) => e.clone(),
        Expr::Tpl(sql, args) => Expr::Tpl(sql.clone(), args.iter().map(swap_sides).collect()),
        Expr::In(value, list, negated) => Expr::In(
            Box::new(swap_sides(value)),
            list.iter().map(swap_sides).collect(),
            *negated,
        ),
        Expr::Agg(sql, args, filter, order) => Expr::Agg(
            sql.clone(),
            args.iter().map(swap_sides).collect(),
            filter.as_ref().map(|p| Box::new(swap_sides(p))),
            order.iter().map(|(e, asc)| (swap_sides(e), *asc)).collect(),
        ),
        Expr::Group(k) => Expr::Group(Box::new(swap_sides(k))),
        Expr::Grouping(shape, k) => Expr::Grouping(*shape, Box::new(swap_sides(k))),
        Expr::Win(sql, args, spec) => Expr::Win(
            sql.clone(),
            args.iter().map(swap_sides).collect(),
            Box::new(WinSpec {
                partition: spec.partition.iter().map(swap_sides).collect(),
                order: spec
                    .order
                    .iter()
                    .map(|(k, asc)| (swap_sides(k), *asc))
                    .collect(),
                frame: spec.frame,
            }),
        ),
    }
}

/// The grouping keys an expression carries, in discovery order.
///
/// Mirrors `lower::collect_groups`, including its rule that a `group`/`grouping`
/// key is not descended into: the key's own expression is a column, and a
/// `rollup` inside a key is not a second key.
fn collect_groups<'a>(e: &'a Expr, out: &mut Vec<(Option<GroupingShape>, &'a Expr)>) {
    match e {
        Expr::Group(k) => return push_group(None, k, out),
        Expr::Grouping(shape, k) => return push_group(Some(*shape), k, out),
        _ => {}
    }
    for c in e.children() {
        collect_groups(c, out);
    }
}

fn push_group<'a>(
    shape: Option<GroupingShape>,
    k: &'a Expr,
    out: &mut Vec<(Option<GroupingShape>, &'a Expr)>,
) {
    if !out.iter().any(|(s, x)| *s == shape && *x == k) {
        out.push((shape, k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagara_hir::ir::{Bound, Frame, FrameUnit};

    /// Every branch of a `UNION ALL` chain, however the fold nested it.
    fn branches(r: &Rel) -> Vec<&Rel> {
        match r.bare() {
            Rel::Set {
                kind: SetKind::UnionAll,
                left,
                right,
            } => {
                let mut out = branches(left);
                out.extend(branches(right));
                out
            }
            _ => vec![r],
        }
    }

    fn table(name: &str, cols: &[&str]) -> Rel {
        Rel::Table {
            schema: "m".into(),
            name: name.into(),
            columns: Some(cols.iter().map(|c| (*c).to_string()).collect()),
        }
    }

    fn col(n: &str) -> Expr {
        Expr::Col(Side::Single, n.into())
    }

    fn left_col(n: &str) -> Expr {
        Expr::Col(Side::Left, n.into())
    }

    fn right_col(n: &str) -> Expr {
        Expr::Col(Side::Right, n.into())
    }

    /// The full join becomes a left join unioned with the right rows that have no
    /// match, and the predicate's sides are swapped in the second branch.
    #[test]
    fn a_full_join_becomes_a_union_of_the_two_outer_halves() {
        let on = Expr::Tpl("$1 = $2".into(), vec![left_col("id"), right_col("uid")]);
        let r = Rel::Join {
            kind: JoinKind::Full,
            left: Box::new(table("l", &["id"])),
            right: Box::new(table("r", &["uid", "v"])),
            on,
        };
        let out = emulate(&r, Dialect::Mysql);
        let Rel::Set {
            kind: SetKind::UnionAll,
            left,
            right,
        } = out.bare()
        else {
            panic!("not a union: {out:?}");
        };
        assert!(matches!(
            left.bare(),
            Rel::Join {
                kind: JoinKind::Left,
                ..
            }
        ));
        let Rel::Select(anti, fields) = right.bare() else {
            panic!("the second branch is not a projection: {right:?}");
        };
        let Rel::Join {
            kind: JoinKind::Anti,
            left: anti_left,
            right: anti_right,
            on,
        } = anti.bare()
        else {
            panic!("the second branch is not an anti-join: {anti:?}");
        };
        // The anti-join's left input is the right relation, and its predicate is
        // the original with the sides exchanged.
        assert!(matches!(anti_left.bare(), Rel::Table { name, .. } if name == "r"));
        assert!(matches!(anti_right.bare(), Rel::Table { name, .. } if name == "l"));
        assert_eq!(
            *on,
            Expr::Tpl("$1 = $2".into(), vec![right_col("id"), left_col("uid")])
        );
        // The full join's row is `id, uid, v`; the left's `id` is NULL here.
        assert_eq!(
            fields,
            &vec![
                ("id".to_string(), null()),
                ("uid".to_string(), col("uid")),
                ("v".to_string(), col("v")),
            ]
        );
        // And the union's two branches have the same row, which is what the
        // validator would otherwise refuse.
        assert_eq!(columns(left), columns(right));
    }

    /// A target that has `FULL JOIN` keeps it: emulation is not applied blindly.
    #[test]
    fn a_native_full_join_is_left_alone() {
        let r = Rel::Join {
            kind: JoinKind::Full,
            left: Box::new(table("l", &["id"])),
            right: Box::new(table("r", &["uid"])),
            on: Expr::Tpl("$1 = $2".into(), vec![left_col("id"), right_col("uid")]),
        };
        for d in [Dialect::Postgres, Dialect::DuckDb, Dialect::Sqlite] {
            assert!(matches!(
                emulate(&r, d).bare(),
                Rel::Join {
                    kind: JoinKind::Full,
                    ..
                }
            ));
        }
    }

    /// `ROLLUP(a, b)` is three sets: `(a, b)`, `(a)` and `()` — not two, and not
    /// `(a, b)` alone.
    #[test]
    fn a_rollup_becomes_one_stage_per_grouping_set() {
        let agg = Rel::Agg(
            Box::new(table("t", &["a", "b", "n"])),
            vec![
                (
                    "a".into(),
                    Expr::Grouping(GroupingShape::Rollup, Box::new(col("a"))),
                ),
                (
                    "b".into(),
                    Expr::Grouping(GroupingShape::Rollup, Box::new(col("b"))),
                ),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None, vec![])),
            ],
        );
        let out = emulate(&agg, Dialect::Sqlite);
        let branches = branches(&out);
        assert_eq!(branches.len(), 3, "{out:#?}");
        // The first keeps both keys, the second only `a`, the third neither.
        let shape = |r: &Rel| -> Vec<String> {
            let Rel::Agg(_, fs) = r.bare() else {
                panic!("not an aggregate: {r:?}")
            };
            fs.iter()
                .map(|(n, e)| {
                    if matches!(e, Expr::Group(..)) {
                        format!("{n}=group")
                    } else if matches!(e, Expr::Tpl(..)) {
                        format!("{n}=null")
                    } else {
                        format!("{n}=other")
                    }
                })
                .collect()
        };
        assert_eq!(shape(branches[0]), ["a=group", "b=group", "n=other"]);
        assert_eq!(shape(branches[1]), ["a=group", "b=null", "n=other"]);
        assert_eq!(shape(branches[2]), ["a=null", "b=null", "n=other"]);
    }

    /// `CUBE(a, b)` is every subset: four sets, including the two singletons.
    #[test]
    fn a_cube_becomes_one_stage_per_subset() {
        let agg = Rel::Agg(
            Box::new(table("t", &["a", "b", "n"])),
            vec![
                (
                    "a".into(),
                    Expr::Grouping(GroupingShape::Cube, Box::new(col("a"))),
                ),
                (
                    "b".into(),
                    Expr::Grouping(GroupingShape::Cube, Box::new(col("b"))),
                ),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None, vec![])),
            ],
        );
        let out = emulate(&agg, Dialect::Sqlite);
        let branches = branches(&out);
        assert_eq!(branches.len(), 4, "{out:#?}");
        // Every subset is there exactly once, including the two singletons and the
        // grand total — `CUBE` is not `ROLLUP`, and the difference is the `b`-only
        // row.
        let kept = |r: &Rel| -> (bool, bool) {
            let Rel::Agg(_, fs) = r.bare() else {
                panic!("not an aggregate: {r:?}")
            };
            let grouped = |name: &str| {
                fs.iter()
                    .any(|(n, e)| n == name && matches!(e, Expr::Group(..)))
            };
            (grouped("a"), grouped("b"))
        };
        let mut seen: Vec<(bool, bool)> = branches.iter().map(|r| kept(r)).collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            [(false, false), (false, true), (true, false), (true, true)],
            "{out:#?}"
        );
    }

    /// A plain key is in every set; a rolled-up key is in some.
    #[test]
    fn a_plain_key_is_in_every_set() {
        let agg = Rel::Agg(
            Box::new(table("t", &["k", "a", "n"])),
            vec![
                ("k".into(), Expr::Group(Box::new(col("k")))),
                (
                    "a".into(),
                    Expr::Grouping(GroupingShape::Rollup, Box::new(col("a"))),
                ),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None, vec![])),
            ],
        );
        let out = emulate(&agg, Dialect::Sqlite);
        let branches = branches(&out);
        assert_eq!(branches.len(), 2, "{out:#?}");
        for branch in branches {
            let Rel::Agg(_, fs) = branch.bare() else {
                panic!("not an aggregate: {branch:?}")
            };
            assert!(
                matches!(&fs[0].1, Expr::Group(k) if **k == col("k")),
                "the plain key must stay grouped in every set: {fs:?}"
            );
        }
    }

    /// A frame's unit is not a grouping set, and a `rows` frame is untouched.
    #[test]
    fn an_ordinary_aggregate_is_left_alone() {
        let agg = Rel::Agg(
            Box::new(table("t", &["a", "n"])),
            vec![
                ("a".into(), Expr::Group(Box::new(col("a")))),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None, vec![])),
            ],
        );
        for d in [Dialect::Sqlite, Dialect::Mysql, Dialect::Postgres] {
            assert!(matches!(emulate(&agg, d).bare(), Rel::Agg(..)));
        }
        // A windowed aggregate with a `rows` frame is not a grouping set either.
        let win = Rel::Select(
            Box::new(table("t", &["a", "n"])),
            vec![(
                "s".into(),
                Expr::Win(
                    "SUM".into(),
                    vec![col("n")],
                    Box::new(WinSpec {
                        partition: vec![],
                        order: vec![(col("a"), true)],
                        frame: Some(Frame {
                            unit: FrameUnit::Rows,
                            start: Bound::Preceding(1),
                            end: Bound::CurrentRow,
                        }),
                    }),
                ),
            )],
        );
        assert!(matches!(
            emulate(&win, Dialect::Sqlite).bare(),
            Rel::Select(..)
        ));
    }

    /// The location wrapper survives, so a diagnostic still points at the stage.
    #[test]
    fn the_location_survives() {
        let loc = cagara_hir::ir::Loc {
            module: 3,
            span: cagara_hir::ir::Span { start: 4, end: 9 },
        };
        let r = Rel::At(
            loc,
            Box::new(Rel::Join {
                kind: JoinKind::Full,
                left: Box::new(table("l", &["id"])),
                right: Box::new(table("r", &["uid"])),
                on: Expr::Tpl("$1 = $2".into(), vec![left_col("id"), right_col("uid")]),
            }),
        );
        match emulate(&r, Dialect::Mysql) {
            Rel::At(got, inner) => {
                assert_eq!(got, loc);
                assert!(matches!(inner.bare(), Rel::Set { .. }));
            }
            other => panic!("lost the wrapper: {other:?}"),
        }
    }

    /// Two shapes in one clause are their cross product, not one merged list.
    ///
    /// `GROUP BY ROLLUP(a), CUBE(c)` groups by every combination of a rollup set
    /// and a cube set — four stages for two keys — because that is what the clause
    /// means. Reading it as "a rollup of a and c" would answer a different
    /// question, and it is the reading a family-based shortcut would produce.
    #[test]
    fn two_shapes_multiply() {
        let agg = Rel::Agg(
            Box::new(table("t", &["a", "c", "n"])),
            vec![
                (
                    "a".into(),
                    Expr::Grouping(GroupingShape::Rollup, Box::new(col("a"))),
                ),
                (
                    "c".into(),
                    Expr::Grouping(GroupingShape::Cube, Box::new(col("c"))),
                ),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None, vec![])),
            ],
        );
        let out = emulate(&agg, Dialect::Sqlite);
        // `ROLLUP(a)` is `{(a), ()}` and `CUBE(c)` is `{(c), ()}`, so the clause
        // names four sets.
        assert_eq!(branches(&out).len(), 4, "{out:#?}");
    }
}
