//! ADVERSARIAL probe (verifier-owned). NOT wired into the crate and NOT part
//! of `cargo test`; it lives under `verify/` on purpose so it cannot be
//! mistaken for the crate's own coverage.
//!
//! Run with:
//!   cp verify/checked_adversarial_probe.rs \
//!      crates/cagara-hir/src/checked/adversarial_probe.rs
//!   # add `mod adversarial_probe;` under the existing `#[cfg(test)] mod tests;`
//!   nix develop --command cargo test -p cagara-hir --lib adversarial_probe
//!
//! Attack C: for every `CheckedQueryNode` constructor, is there an input where
//! the constructor returns `Ok` but `schema::schema(&erase(q))` returns `Err`?
//!
//! The crate's own `schema_of_the_erasure_equals_the_recorded_row*` tests use
//! hand-picked happy paths. This probe deliberately composes stages that the
//! hand-picked list does not: nested renames that collide, `omit` followed by
//! a `select` that re-adds the column, joins whose sides share a name, updates
//! whose field list collides with an input column, and renames applied twice.

use super::*;
use crate::core::{Origin, RowType, ScalarType};
use crate::ir::{JoinKind, SetKind};
use cagara_syntax::ast::Span;

fn o() -> Origin {
    Origin::new(0, Span { start: 0, end: 1 })
}

fn col(name: &str, ty: ScalarType) -> CheckedExpr {
    CheckedExpr::column(cagara_syntax::ast::Side::Single, name, ty, o())
}

/// A left-side-qualified column, for join predicates.
fn lcol(name: &str, ty: ScalarType) -> CheckedExpr {
    CheckedExpr::column(cagara_syntax::ast::Side::Left, name, ty, o())
}

/// A constant `bool`, the one predicate form not checked against the sides.
fn lit_bool() -> CheckedExpr {
    CheckedExpr::lit(crate::ir::Lit::Bool(true), o())
}

fn t(schema: &str, name: &str, cols: &[(&str, ScalarType)]) -> CheckedQuery {
    CheckedQuery::table(
        schema,
        name,
        Some(RowType::new(
            cols.iter().map(|(n, ty)| (n.to_string(), ty.clone())).collect(),
        )),
        o(),
    )
    .unwrap_or_else(|_| panic!("building table {name}"))
}

/// The single invariant the whole design rests on.
fn invariant(q: &CheckedQuery) -> Result<(), String> {
    let erased = crate::schema::schema(&q.clone().erase());
    match erased {
        Err(e) => Err(format!("erase->schema REJECTED a constructible query: {e}")),
        Ok(cols) => {
            let row: Vec<String> = q.row.columns().iter().map(|(n, _)| n.clone()).collect();
            if cols != row {
                Err(format!("row {row:?} != erased schema {cols:?}"))
            } else {
                Ok(())
            }
        }
    }
}

/// Every query this probe could compose, to be checked in bulk.
fn cases() -> Vec<(&'static str, CheckedQuery)> {
    let mut out: Vec<(&'static str, CheckedQuery)> = Vec::new();
    let base = || t("s", "t", &[("a", ScalarType::Int), ("b", ScalarType::String)]);

    // ── the hand-picked happy paths the crate already covers ──────────────
    out.push(("table", base()));
    out.push(("where", CheckedQuery::where_(base(), col("a", ScalarType::Bool), o()).unwrap()));
    out.push(("select", CheckedQuery::select(base(), vec![("x".into(), col("a", ScalarType::Int))], o()).unwrap()));
    out.push(("omit", CheckedQuery::omit(base(), "b", o()).unwrap()));
    out.push(("prefix", CheckedQuery::prefix(base(), "u_", o()).unwrap()));

    // ── ATTACK 1: rename that collides with an existing column name ───────
    // `prefix "u_"` on [u_a, a] would produce [u_u_a, u_a] -- no collision.
    // But suffix with an affix that makes two names equal is impossible since
    // rename_all is injective. The real risk is a rename colliding with a
    // LATER join's other side. Compose it.
    let renamed = CheckedQuery::prefix(base(), "b", o()).unwrap();
    out.push(("prefix-then-join-self-name", renamed.clone()));
    let other = t("s", "u", &[("ba", ScalarType::Int), ("bb", ScalarType::Int)]);
    if let Ok(j) = CheckedQuery::join(
        JoinKind::Inner,
        renamed.clone(),
        other.clone(),
        col("a", ScalarType::Bool),
        o(),
    ) {
        out.push(("join-after-prefix", j));
    }

    // ── ATTACK 2: rename applied twice ───────────────────────────────────
    let twice = CheckedQuery::prefix(CheckedQuery::suffix(base(), "_x", o()).unwrap(), "y_", o());
    if let Ok(q) = twice {
        out.push(("prefix-over-suffix", q));
    }

    // ── ATTACK 3: omit a column a later select re-adds ───────────────────
    if let Ok(om) = CheckedQuery::omit(base(), "a", o()) {
        if let Ok(sel) = CheckedQuery::select(om, vec![("a".into(), col("b", ScalarType::String))], o()) {
            out.push(("omit-then-readd", sel));
        }
    }

    // ── ATTACK 4: update whose field collides with an input column ───────
    if let Ok(u) = CheckedQuery::update(base(), vec![("a".into(), col("b", ScalarType::String))], o()) {
        out.push(("update-collide", u));
    }
    // ...and the reverse (type going the other way)
    if let Ok(u) = CheckedQuery::update(base(), vec![("b".into(), col("a", ScalarType::Int))], o()) {
        out.push(("update-collide-reverse", u));
    }

    // ── ATTACK 5: joins whose two sides share a column name ──────────────
    let left = t("s", "l", &[("id", ScalarType::Int)]);
    let right = t("s", "r", &[("id", ScalarType::String), ("m", ScalarType::Maybe(Box::new(ScalarType::Int)))]);
    for (label, kind) in [
        ("join-inner", JoinKind::Inner),
        ("join-left", JoinKind::Left),
        ("join-right", JoinKind::Right),
        ("join-full", JoinKind::Full),
        ("join-semi", JoinKind::Semi),
        ("join-anti", JoinKind::Anti),
    ] {
        // A join predicate must name a side, so build a side-qualified bool
        // reference; if the sides do not accept it, fall back to a literal so
        // the case still composes and gets reported rather than silently
        // skipped.
        let on = lcol("id", ScalarType::Bool);
        if let Ok(j) = CheckedQuery::join(kind, left.clone(), right.clone(), on, o()) {
            out.push((label, j));
        } else if let Ok(j) =
            CheckedQuery::join(kind, left.clone(), right.clone(), lit_bool(), o())
        {
            out.push((label, j));
        }
    }

    // ── ATTACK 6: nested joins, each with a renamed side ─────────────────
    if let Ok(j1) = CheckedQuery::join(
        JoinKind::Left,
        left.clone(),
        right.clone(),
        lcol("id", ScalarType::Bool),
        o(),
    ) {
        if let Ok(renamed) = CheckedQuery::prefix(right.clone(), "id", o()) {
            if let Ok(j2) =
                CheckedQuery::join(JoinKind::Inner, j1, renamed, lcol("id", ScalarType::Bool), o())
            {
                out.push(("nested-joins-renamed-side", j2));
            }
        }
    }

    // ── ATTACK 7: set over rows that MATCH ───────────────────────────────
    // The mismatched case is the dedicated negative probe below (the
    // constructor correctly rejects it, so it cannot be a positive case).
    if let Ok(a) = CheckedQuery::select(base(), vec![("x".into(), col("a", ScalarType::Int))], o()) {
        if let Ok(b) = CheckedQuery::select(base(), vec![("x".into(), col("a", ScalarType::Int))], o()) {
            if let Ok(s) = CheckedQuery::set(SetKind::Union, a, b, o()) {
                out.push(("set-matching-rows", s));
            }
        }
    }

    // ── ATTACK 8: agg over a const field, then select it ─────────────────
    // A bare row column in `agg` is correctly rejected (needs `group`), so
    // the composable positive case is the const field.
    if let Ok(ag) = CheckedQuery::agg(
        base(),
        vec![("k".into(), CheckedExpr::lit(crate::ir::Lit::Int(1), o()))],
        o(),
    ) {
        out.push(("agg-const-field", ag.clone()));
        if let Ok(sel) = CheckedQuery::select(ag, vec![("z".into(), col("k", ScalarType::Int))], o()) {
            out.push(("select-after-agg", sel));
        }
    }

    // ── ATTACK 9: empty-affix rename (identity) ──────────────────────────
    if let Ok(q) = CheckedQuery::prefix(base(), "", o()) {
        out.push(("prefix-empty-affix", q));
    }

    // ── ATTACK 10: limit/offset/distinct/order stacking ──────────────────
    if let Ok(q) = CheckedQuery::limit(
        CheckedQuery::offset(CheckedQuery::distinct(CheckedQuery::order(base(), vec![(col("a", ScalarType::Int), true)], o()).unwrap(), o()).unwrap(), 5, o()).unwrap(),
        10, o(),
    ) {
        out.push(("stacked-unary", q));
    }

    out
}

#[test]
fn every_constructible_query_satisfies_row_equals_erased_schema() {
    let cs = cases();
    // Non-vacuity guard: the `if let Ok` composition below can silently skip a
    // case, so report how many actually composed and which.
    eprintln!(
        "PROBE: {} cases composed: {:?}",
        cs.len(),
        cs.iter().map(|(l, _)| *l).collect::<Vec<_>>()
    );
    let mut failures = Vec::new();
    for (label, q) in cs {
        if let Err(e) = invariant(&q) {
            failures.push(format!("  [{label}] {e}"));
        }
    }
    assert!(
        failures.is_empty(),
        "attack C found {} constructible query/queries that the validator rejects \
         or whose row disagrees with the erasure:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Attack C, the part the crate's own tests cannot express: *call* every
/// constructor with deliberately hostile arguments and record which ones
/// return `Ok` even though `schema::schema` rejects the erasure. A constructor
/// in that set is weaker than the validator it claims to subsume.
#[test]
fn no_constructor_returns_ok_where_the_validator_rejects() {
    let base = || t("s", "t", &[("a", ScalarType::Int), ("b", ScalarType::String)]);
    let mut holes: Vec<String> = Vec::new();

    // omit a key that is not there
    if let Ok(q) = CheckedQuery::omit(base(), "nope", o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("omit(missing key) returns Ok but schema rejects".into());
        }
    }
    // order key reading a column that is not there
    if let Ok(q) = CheckedQuery::order(base(), vec![(col("nope", ScalarType::Int), true)], o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("order(unknown column) returns Ok but schema rejects".into());
        }
    }
    // where over an unknown column
    if let Ok(q) = CheckedQuery::where_(base(), col("nope", ScalarType::Bool), o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("where(unknown column) returns Ok but schema rejects".into());
        }
    }
    // select over an unknown column
    if let Ok(q) = CheckedQuery::select(base(), vec![("x".into(), col("nope", ScalarType::Int))], o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("select(unknown column) returns Ok but schema rejects".into());
        }
    }
    // join predicate naming a column absent from its own side
    let l = t("s", "l", &[("id", ScalarType::Int)]);
    let r = t("s", "r", &[("id", ScalarType::Int)]);
    if let Ok(q) = CheckedQuery::join(JoinKind::Inner, l.clone(), r.clone(), col("nope", ScalarType::Bool), o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("join(unknown column in predicate) returns Ok but schema rejects".into());
        }
    }
    // set over rows that do not match
    let a = t("s", "a", &[("x", ScalarType::Int)]);
    let b = t("s", "b", &[("y", ScalarType::Int)]);
    if let Ok(q) = CheckedQuery::set(SetKind::Union, a, b, o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("set(mismatched rows) returns Ok but schema rejects".into());
        }
    }
    // empty field lists
    if let Ok(q) = CheckedQuery::select(base(), vec![], o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("select(empty) returns Ok but schema rejects".into());
        }
    }
    if let Ok(q) = CheckedQuery::update(base(), vec![], o()) {
        if crate::schema::schema(&q.clone().erase()).is_err() {
            holes.push("update(empty) returns Ok but schema rejects".into());
        }
    }

    assert!(
        holes.is_empty(),
        "attack C: {} constructor(s) weaker than `schema::schema`:\n  {}",
        holes.len(),
        holes.join("\n  ")
    );
}
