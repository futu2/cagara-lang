//! One traversal of a relation, used by every capability check.
//!
//! The checks in [`crate::compile_located`] ask "does this query contain X
//! anywhere", and the answer has to be about the *whole* query. The first
//! version of them answered it about part of the query: each check matched
//! `Rel::Select` and `Rel::Agg` and inspected that node's own expressions,
//! falling through to `rel.children()` for everything else — so a `Select` or an
//! `Agg` was a leaf as far as the check was concerned, and its input was never
//! looked at. A `rollup` behind a later `agg` therefore compiled for SQLite,
//! which has no `ROLLUP`, and the failure moved from the compiler to the server:
//!
//! ```text
//! main = f & agg { k = rollup .k, n = count } & agg { total = count }
//! -- compiles for --dialect sqlite, then SQLite answers: no such function: ROLLUP
//! ```
//!
//! The bug was not the missing case — it was that *each check wrote its own
//! walk*, and each one could forget a node independently. So the walk lives here
//! once. A check is a predicate over [`Expr`]s (and, for the few constructs that
//! are nodes rather than expressions, over [`Rel`]s), and the traversal is
//! shared: a node added to [`Rel`] is a compile error in `own_exprs` below, and
//! an expression form added to [`Expr`] is already covered because `Expr::any`
//! is `cagara-hir`'s own exhaustive walk.
//!
//! # What "the whole query" means
//!
//! Both children *and* the node's own expressions, at every level: a `Select`
//! contributes its fields and its input, an `Agg` its fields and its input, a
//! `Join` its `on` predicate and both sides, and so on. Nothing is skipped,
//! including the expressions that are not in a `Select` at all — a `where`, an
//! `order` key, an `unnest` argument, a `markExists` predicate.

use cagara_hir::ir::{Expr, Rel};

/// Every expression a node carries **itself**, in no particular order.
///
/// Children are not included: [`each_expr`] supplies them by descending. This is
/// exhaustive over `Rel` on purpose — a new node with an expression in it must be
/// added here or the crate does not compile, which is what keeps the traversal
/// honest when the IR grows.
pub(crate) fn own_exprs(rel: &Rel) -> Vec<&Expr> {
    match rel {
        Rel::Table { .. } => vec![],
        Rel::Where(_, e) => vec![e],
        Rel::Select(_, fs) | Rel::Update(_, fs) | Rel::Agg(_, fs) => {
            fs.iter().map(|(_, e)| e).collect()
        }
        Rel::Omit(_, _) | Rel::Prefix(_, _) | Rel::Suffix(_, _) => vec![],
        Rel::Unnest(_, e) => vec![e],
        Rel::Order(_, ks) => ks.iter().map(|(e, _)| e).collect(),
        Rel::Limit(_, _) | Rel::Offset(_, _) | Rel::Distinct(_) | Rel::At(_, _) => vec![],
        Rel::Join { on, .. } => vec![on],
        Rel::Set { .. } => vec![],
        Rel::MarkExists { on, .. } => vec![on],
    }
}

/// Whether one node's **own** expressions satisfy `p`, without descending.
///
/// The predicate [`locate`] needs: a check about a construct has to blame the
/// stage the construct is written in, and a subtree test would blame the root.
pub(crate) fn node_has_expr(rel: &Rel, p: &dyn Fn(&Expr) -> bool) -> bool {
    own_exprs(rel).into_iter().any(|e| e.any(p))
}

/// Whether any expression anywhere in `rel` satisfies `p`.
///
/// The predicate is applied to a node's own expressions *and* to every
/// descendant of each expression (`Expr::any` walks the expression tree), so a
/// `groups` frame inside an arithmetic expression inside a `select` is found.
pub(crate) fn any_expr(rel: &Rel, p: &dyn Fn(&Expr) -> bool) -> bool {
    own_exprs(rel).into_iter().any(|e| e.any(p))
        || rel.children().into_iter().any(|c| any_expr(c, p))
}

/// Whether any node in `rel` satisfies `p`, including `At`-wrapped ones.
///
/// `p` sees the node with its `At` wrappers stripped, because every caller here
/// is asking about the construct rather than about how it was written; the
/// wrapper is what [`crate::locate`] reads.
pub(crate) fn any_rel(rel: &Rel, p: &dyn Fn(&Rel) -> bool) -> bool {
    p(rel.bare()) || rel.children().into_iter().any(|c| any_rel(c, p))
}

/// The location of the innermost node `p` accepts, or of the innermost node at all.
///
/// The dialect refusals in `compile_located` are about a *construct* — a `GROUPS`
/// frame, a `list` value — and the reader needs to be sent to where that
/// construct was written. `Rel::At` is the only carrier of a span, so this
/// descends the tree keeping the deepest one it has seen and, when the predicate
/// matches, reports the location in force at that node: the same "innermost
/// enclosing stage" rule `cagara_hir::schema` uses.
///
/// Returns `None` for a relation with no `At` anywhere, which is what an IR built
/// by hand in a test looks like. A caller then prints no position rather than a
/// wrong one.
pub(crate) fn locate(rel: &Rel, p: &dyn Fn(&Rel) -> bool) -> Option<cagara_hir::ir::Loc> {
    fn walk(
        rel: &Rel,
        p: &dyn Fn(&Rel) -> bool,
        here: Option<cagara_hir::ir::Loc>,
        best: &mut Option<cagara_hir::ir::Loc>,
    ) {
        let here = match rel {
            Rel::At(l, _) => Some(*l),
            _ => here,
        };
        if p(rel) && best.is_none() {
            *best = here;
        }
        for child in rel.children() {
            walk(child, p, here, best);
        }
    }
    let mut best = None;
    walk(rel, p, None, &mut best);
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagara_hir::ir::{Frame, FrameUnit, Lit, Side, WinSpec};

    fn table() -> Rel {
        Rel::Table {
            schema: "m".into(),
            name: "t".into(),
            columns: Some(vec!["k".into(), "n".into()]),
        }
    }

    fn framed(unit: FrameUnit) -> Expr {
        Expr::Win(
            "SUM".into(),
            vec![Expr::Col(Side::Single, "n".into())],
            Box::new(WinSpec {
                partition: vec![],
                order: vec![(Expr::Col(Side::Single, "k".into()), true)],
                frame: Some(Frame {
                    unit,
                    start: cagara_hir::ir::Bound::Preceding(1),
                    end: cagara_hir::ir::Bound::CurrentRow,
                }),
            }),
        )
    }

    /// The regression this module exists for: the frame is inside a `Select`,
    /// and the `Select` is the *input* of another stage.
    #[test]
    fn a_frame_behind_a_projection_is_found() {
        let inner = Rel::Select(
            Box::new(table()),
            vec![("s".into(), framed(FrameUnit::Groups))],
        );
        let outer = Rel::Select(
            Box::new(inner),
            vec![("s".into(), Expr::Col(Side::Single, "s".into()))],
        );
        assert!(any_expr(&outer, &|e| matches!(
            e,
            Expr::Win(_, _, spec)
                if spec.frame.is_some_and(|f| f.unit == FrameUnit::Groups)
        )));
        // And a `Rows` frame is not a `Groups` one.
        let rows = Rel::Select(
            Box::new(table()),
            vec![("s".into(), framed(FrameUnit::Rows))],
        );
        let outer = Rel::Select(
            Box::new(rows),
            vec![("s".into(), Expr::Col(Side::Single, "s".into()))],
        );
        assert!(!any_expr(&outer, &|e| matches!(
            e,
            Expr::Win(_, _, spec)
                if spec.frame.is_some_and(|f| f.unit == FrameUnit::Groups)
        )));
    }

    /// A grouping key behind a second `agg` — the reported SQLite bug.
    #[test]
    fn a_grouping_set_behind_an_aggregate_is_found() {
        let inner = Rel::Agg(
            Box::new(table()),
            vec![
                (
                    "k".into(),
                    Expr::Grouping(
                        cagara_hir::ir::GroupingShape::Rollup,
                        Box::new(Expr::Col(Side::Single, "k".into())),
                    ),
                ),
                ("n".into(), Expr::Agg("COUNT".into(), vec![], None)),
            ],
        );
        let outer = Rel::Agg(
            Box::new(inner),
            vec![("total".into(), Expr::Agg("COUNT".into(), vec![], None))],
        );
        assert!(any_expr(&outer, &|e| matches!(e, Expr::Grouping(..))));
    }

    /// Every expression slot is reached, not only the ones on a `Select`.
    #[test]
    fn every_expression_slot_is_reached() {
        let marked = |e: Expr| e.any(&|x| matches!(x, Expr::Lit(Lit::Int(99))));
        let lit = Expr::Lit(Lit::Int(99));
        let cases: Vec<Rel> = vec![
            Rel::Where(Box::new(table()), lit.clone()),
            Rel::Select(Box::new(table()), vec![("x".into(), lit.clone())]),
            Rel::Update(Box::new(table()), vec![("x".into(), lit.clone())]),
            Rel::Agg(Box::new(table()), vec![("x".into(), lit.clone())]),
            Rel::Order(Box::new(table()), vec![(lit.clone(), true)]),
            Rel::Unnest(Box::new(table()), lit.clone()),
            Rel::Join {
                kind: cagara_hir::ir::JoinKind::Inner,
                left: Box::new(table()),
                right: Box::new(table()),
                on: lit.clone(),
            },
            Rel::MarkExists {
                input: Box::new(table()),
                right: Box::new(table()),
                on: lit.clone(),
            },
        ];
        for r in cases {
            assert!(any_expr(&r, &|e| marked(e.clone())), "{r:?}");
        }
    }

    /// A node with no expressions of its own is still descended through.
    #[test]
    fn wrappers_are_transparent() {
        let inner = Rel::Where(Box::new(table()), Expr::Lit(Lit::Int(99)));
        let wrapped = Rel::At(
            cagara_hir::ir::Loc {
                module: 0,
                span: cagara_hir::ir::Span { start: 0, end: 1 },
            },
            Box::new(Rel::Distinct(Box::new(Rel::Limit(Box::new(inner), 3)))),
        );
        assert!(any_expr(&wrapped, &|e| matches!(
            e,
            Expr::Lit(Lit::Int(99))
        )));
        assert!(any_rel(&wrapped, &|r| matches!(r, Rel::Limit(..))));
    }
}
