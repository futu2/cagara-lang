//! `Rel -> Rel` rewrites, applied before lowering.
//!
//! The lowerer can already fold a stage into the current `SELECT` or wrap it in
//! a derived table, and that is enough for most pipelines. What it cannot do is
//! *look at two stages at once*: whether `limit 3 & limit 2` needs two `SELECT`s
//! is a question about a pair of nodes, not about one node against the stage
//! built so far. That is what this pass is for, and every rule here removes a
//! derived table or fixes an answer — nothing in it is cosmetic.
//!
//! The pass is semantics-preserving by construction and is checked as such: the
//! differential suite runs every shared case through the rewritten lowering on
//! SQLite and DuckDB (see `tests/engines.rs`), so a rule that changes an answer
//! shows up as a wrong row rather than as a comment.
//!
//! It knows nothing about dialects, and nothing here mentions SQL: the output is
//! another `Rel`, which the lowerer turns into stages exactly as before.

use cagara_hir::ir::Rel;

/// The relation after every rule, applied bottom-up.
///
/// Bottom-up so a chain collapses in one traversal: the children of
/// `limit 3 & limit 2 & limit 1` are already one `Limit` by the time the outer
/// one is looked at.
///
/// Children are rewritten once, and again only after a rule has *rebuilt* this
/// node — a rebuild can put a collapsible child back in place. Rewriting them on
/// every pass instead would double the work per level, which is exponential in
/// the depth of a filter chain and was measured: the two deep-chain tests in
/// `tests.rs` did not finish.
pub fn rewrite(rel: &Rel) -> Rel {
    let mut r = rel.clone();
    for child in r.children_mut() {
        *child = rewrite(child);
    }
    loop {
        match step(&r) {
            None => return r,
            Some(next) => {
                r = next;
                for child in r.children_mut() {
                    *child = rewrite(child);
                }
            }
        }
    }
}

/// One rule on one node.
fn step(r: &Rel) -> Option<Rel> {
    // Every node carries its location, and a rule that rebuilds a node has to
    // keep it: the location is what a diagnostic points at, and it is the outer
    // node's that names the stage the reader wrote.
    let loc = match &r {
        Rel::At(loc, _) => Some(*loc),
        _ => None,
    };
    let collapsed = match r.bare() {
        // Two limits keep the smaller: the first `n` rows of the first `m` rows
        // are the first `min(n, m)` rows.
        Rel::Limit(outer, b) => match outer.bare() {
            Rel::Limit(src, a) => Some(Rel::Limit(src.clone(), *a.min(b))),
            _ => None,
        },
        Rel::Offset(outer, b) => match outer.bare() {
            // Two offsets add: skipping `a` and then `b` skips `a + b`.
            Rel::Offset(src, a) => Some(Rel::Offset(src.clone(), *a + *b)),
            // A limit and then an offset keeps rows `m+1 ..= n`, which is one
            // stage: skip `m`, then keep `n - m`. Folding the pair into a single
            // `LIMIT n OFFSET m` — which is what the lowerer does with a `limit`
            // followed by an `offset`, since they are one clause in SQL — keeps
            // `m` rows too many.
            Rel::Limit(src, n) => {
                let (n, m) = (*n, *b);
                Some(if n <= m {
                    Rel::Limit(src.clone(), 0)
                } else {
                    Rel::Limit(Box::new(Rel::Offset(src.clone(), m)), n - m)
                })
            }
            _ => None,
        },
        // Deduping a deduped relation is deduping it.
        Rel::Distinct(outer) => match outer.bare() {
            Rel::Distinct(src) => Some(Rel::Distinct(src.clone())),
            _ => None,
        },
        _ => None,
    };
    let body = collapsed?;
    Some(match loc {
        Some(loc) => Rel::At(loc, Box::new(body)),
        None => body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagara_hir::ir::Rel;

    fn table() -> Rel {
        Rel::Table {
            schema: "m".into(),
            name: "t".into(),
            columns: None,
        }
    }

    /// The shape of a rewritten relation, without locations, for asserting on.
    fn shape(r: &Rel) -> String {
        match r.bare() {
            Rel::Table { name, .. } => name.clone(),
            Rel::Limit(inner, n) => format!("limit {n} ({})", shape(inner)),
            Rel::Offset(inner, n) => format!("offset {n} ({})", shape(inner)),
            Rel::Distinct(inner) => format!("distinct ({})", shape(inner)),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn two_limits_keep_the_smaller() {
        let r = Rel::Limit(Box::new(Rel::Limit(Box::new(table()), 3)), 2);
        assert_eq!(shape(&rewrite(&r)), "limit 2 (t)");
        // Either order: the first `m` of the first `n` is the first `min`.
        let r = Rel::Limit(Box::new(Rel::Limit(Box::new(table()), 2)), 3);
        assert_eq!(shape(&rewrite(&r)), "limit 2 (t)");
    }

    #[test]
    fn two_offsets_add() {
        let r = Rel::Offset(Box::new(Rel::Offset(Box::new(table()), 1)), 2);
        assert_eq!(shape(&rewrite(&r)), "offset 3 (t)");
    }

    #[test]
    fn a_limit_then_an_offset_becomes_one_stage() {
        // Rows 3..=5 of the table: skip 2, keep 3.
        let r = Rel::Offset(Box::new(Rel::Limit(Box::new(table()), 5)), 2);
        assert_eq!(shape(&rewrite(&r)), "limit 3 (offset 2 (t))");
        // An offset past the limit keeps nothing, and says so without a second
        // stage.
        let r = Rel::Offset(Box::new(Rel::Limit(Box::new(table()), 2)), 5);
        assert_eq!(shape(&rewrite(&r)), "limit 0 (t)");
        // Exactly at the end is the same empty result.
        let r = Rel::Offset(Box::new(Rel::Limit(Box::new(table()), 2)), 2);
        assert_eq!(shape(&rewrite(&r)), "limit 0 (t)");
    }

    #[test]
    fn a_chain_collapses_in_one_pass() {
        let r = Rel::Offset(
            Box::new(Rel::Offset(
                Box::new(Rel::Limit(Box::new(Rel::Limit(Box::new(table()), 9)), 4)),
                1,
            )),
            1,
        );
        // Two limits -> 4; two offsets -> 2; limit 4 then offset 2 -> limit 2.
        assert_eq!(shape(&rewrite(&r)), "limit 2 (offset 2 (t))");
    }

    #[test]
    fn deduping_twice_is_deduping_once() {
        let r = Rel::Distinct(Box::new(Rel::Distinct(Box::new(table()))));
        assert_eq!(shape(&rewrite(&r)), "distinct (t)");
    }

    #[test]
    fn the_location_survives() {
        let loc = cagara_hir::ir::Loc {
            module: 7,
            span: cagara_hir::ir::Span { start: 1, end: 2 },
        };
        let r = Rel::At(
            loc,
            Box::new(Rel::Limit(Box::new(Rel::Limit(Box::new(table()), 3)), 2)),
        );
        match rewrite(&r) {
            Rel::At(got, inner) => {
                assert_eq!(got, loc);
                assert!(matches!(inner.bare(), Rel::Limit(..)));
            }
            other => panic!("lost the wrapper: {other:?}"),
        }
    }
}
