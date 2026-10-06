//! Tests for the checked relational layer.
//!
//! Two things are pinned here:
//!
//!   * **positive** — each constructor produces the `Rel` shape today's
//!     evaluator produces for the same program, and erasing a query yields a
//!     `Rel` whose `schema::schema` agrees with the row the constructor
//!     recorded. That agreement is the invariant that ties the checked layer
//!     to the validator;
//!   * **negative** — one test per rule the constructors enforce, so a rule
//!     that stops being enforced fails a test rather than a user.

use super::*;
use crate::core::{Diagnostic, Error, Origin, RowType, ScalarType};
use crate::ir::{JoinKind, Lit, Loc, Phase, Rel, SetKind};
use cagara_syntax::ast::{Side, Span};

fn o() -> Origin {
    Origin::new(1, Span { start: 4, end: 12 })
}

fn span(n: u32) -> Span {
    Span {
        start: n,
        end: n + 1,
    }
}

/// `t : query { a = int, b = string }` as a table the checked layer accepts.
fn table() -> CheckedQuery {
    CheckedQuery::table(
        "s",
        "t",
        Some(RowType::new(vec![
            ("a".into(), ScalarType::Int),
            ("b".into(), ScalarType::String),
        ])),
        o(),
    )
    .unwrap()
}

fn col(name: &str, ty: ScalarType) -> CheckedExpr {
    CheckedExpr::column(Side::Single, name, ty, o())
}

fn int_col(name: &str) -> CheckedExpr {
    col(name, ScalarType::Int)
}

fn bool_col(name: &str) -> CheckedExpr {
    col(name, ScalarType::Bool)
}

fn suffixed(name: &str, ty: ScalarType) -> CheckedExpr {
    CheckedExpr::column(Side::Right, name, ty, o())
}

fn base_rel() -> Rel {
    Rel::At(
        Loc {
            module: 1,
            span: Span { start: 4, end: 12 },
        },
        Box::new(Rel::Table {
            schema: "s".into(),
            name: "t".into(),
            columns: Some(vec!["a".into(), "b".into()]),
        }),
    )
}

// ── positive: accepted shapes erase to the evaluator's `Rel` ───────────────

#[test]
fn table_erases_to_an_at_wrapped_table_with_its_columns() {
    assert_eq!(table().erase(), base_rel());
}

#[test]
fn where_erases_to_rel_where_and_keeps_the_input_row() {
    let q = CheckedQuery::where_(table(), bool_col("a"), o()).unwrap();
    assert_eq!(q.row, table().row);
    assert_eq!(
        q.erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Where(
                Box::new(base_rel()),
                Expr::Col(Side::Single, "a".into())
            ))
        )
    );
}

#[test]
fn select_erases_to_rel_select_and_its_row_is_the_field_list() {
    let q = CheckedQuery::select(
        table(),
        vec![
            ("x".into(), int_col("a")),
            ("y".into(), col("b", ScalarType::String)),
        ],
        o(),
    )
    .unwrap();
    assert_eq!(
        q.row,
        RowType::new(vec![
            ("x".into(), ScalarType::Int),
            ("y".into(), ScalarType::String)
        ])
    );
    assert_eq!(
        q.erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Select(
                Box::new(base_rel()),
                vec![
                    ("x".into(), Expr::Col(Side::Single, "a".into())),
                    ("y".into(), Expr::Col(Side::Single, "b".into())),
                ]
            ))
        )
    );
}

#[test]
fn update_erases_to_rel_update_and_merges_over_the_input() {
    let q = CheckedQuery::update(table(), vec![("a".into(), int_col("b"))], o()).unwrap();
    assert_eq!(
        q.erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Update(
                Box::new(base_rel()),
                vec![("a".into(), Expr::Col(Side::Single, "b".into()))]
            ))
        )
    );
}

#[test]
fn agg_erases_to_rel_agg() {
    let f = CheckedExpr::agg_template("COUNT(*)".to_string(), vec![], ScalarType::Int, o()).unwrap();
    let q = CheckedQuery::agg(table(), vec![("n".into(), f)], o()).unwrap();
    assert_eq!(
        q.row,
        RowType::new(vec![("n".into(), ScalarType::Int)])
    );
    assert_eq!(
        q.erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Agg(
                Box::new(base_rel()),
                vec![("n".into(), Expr::Agg("COUNT(*)".into(), vec![]))]
            ))
        )
    );
}

#[test]
fn order_limit_offset_distinct_omit_and_rename_erase_directly() {
    let loc = Loc {
        module: 1,
        span: Span { start: 4, end: 12 },
    };
    assert_eq!(
        CheckedQuery::order(table(), vec![(int_col("a"), false)], o())
            .unwrap()
            .erase(),
        Rel::At(
            loc,
            Box::new(Rel::Order(
                Box::new(base_rel()),
                vec![(Expr::Col(Side::Single, "a".into()), false)]
            ))
        )
    );
    assert_eq!(
        CheckedQuery::limit(table(), 5, o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Limit(Box::new(base_rel()), 5)))
    );
    assert_eq!(
        CheckedQuery::offset(table(), 2, o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Offset(Box::new(base_rel()), 2)))
    );
    assert_eq!(
        CheckedQuery::distinct(table(), o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Distinct(Box::new(base_rel()))))
    );
    assert_eq!(
        CheckedQuery::omit(table(), "b", o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Omit(Box::new(base_rel()), "b".into())))
    );
    assert_eq!(
        CheckedQuery::prefix(table(), "u_", o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Prefix(Box::new(base_rel()), "u_".into())))
    );
    assert_eq!(
        CheckedQuery::suffix(table(), "_v2", o()).unwrap().erase(),
        Rel::At(loc, Box::new(Rel::Suffix(Box::new(base_rel()), "_v2".into())))
    );
}

#[test]
fn join_erases_to_rel_join_with_a_sided_predicate() {
    let on = CheckedExpr::template(
        "$1 = $2".to_string(),
        vec![
            CheckedExpr::column(Side::Left, "a", ScalarType::Int, o()),
            suffixed("a", ScalarType::Int),
        ],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let q = CheckedQuery::join(JoinKind::Inner, table(), table(), on, o()).unwrap();
    assert_eq!(
        q.erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Join {
                kind: JoinKind::Inner,
                left: Box::new(base_rel()),
                right: Box::new(base_rel()),
                on: Expr::Tpl(
                    "$1 = $2".to_string(),
                    vec![
                        Expr::Col(Side::Left, "a".into()),
                        Expr::Col(Side::Right, "a".into()),
                    ]
                ),
            })
        )
    );
}

#[test]
fn set_erases_to_rel_set() {
    assert_eq!(
        CheckedQuery::set(SetKind::Union, table(), table(), o())
            .unwrap()
            .erase(),
        Rel::At(
            Loc {
                module: 1,
                span: Span { start: 4, end: 12 }
            },
            Box::new(Rel::Set {
                kind: SetKind::Union,
                left: Box::new(base_rel()),
                right: Box::new(base_rel()),
            })
        )
    );
}

#[test]
fn expressions_erase_to_the_ir_expression_they_came_from() {
    assert_eq!(
        erase_expr(int_col("a")).unwrap(),
        Expr::Col(Side::Single, "a".into())
    );
    assert_eq!(
        erase_expr(CheckedExpr::lit(Lit::Int(3), o())).unwrap(),
        Expr::Lit(Lit::Int(3))
    );
    assert_eq!(
        erase_expr(
            CheckedExpr::template("$1 + 1".to_string(), vec![int_col("a")], ScalarType::Int, o()).unwrap()
        )
        .unwrap(),
        Expr::Tpl("$1 + 1".into(), vec![Expr::Col(Side::Single, "a".into())])
    );
    assert_eq!(
        erase_expr(
            CheckedExpr::agg_template("SUM($1)".to_string(), vec![int_col("a")], ScalarType::Int, o())
                .unwrap()
        )
        .unwrap(),
        Expr::Agg("SUM($1)".into(), vec![Expr::Col(Side::Single, "a".into())])
    );
    assert_eq!(
        erase_expr(CheckedExpr::group(int_col("a"), o()).unwrap()).unwrap(),
        Expr::Group(Box::new(Expr::Col(Side::Single, "a".into())))
    );
    assert_eq!(
        erase_expr(
            CheckedExpr::in_(int_col("a"), vec![CheckedExpr::lit(Lit::Int(1), o())], false, o())
                .unwrap()
        )
        .unwrap(),
        Expr::In(
            Box::new(Expr::Col(Side::Single, "a".into())),
            vec![Expr::Lit(Lit::Int(1))],
            false
        )
    );
}

#[test]
fn a_window_template_erases_to_ir_win_with_its_spec() {
    let frame = crate::ir::Frame {
        start: crate::ir::Bound::UnboundedPreceding,
        end: crate::ir::Bound::CurrentRow,
    };
    let spec = window_spec(vec![int_col("a")], vec![(int_col("b"), true)], Some(frame)).unwrap();
    let w =
        CheckedExpr::win_template("ROW_NUMBER()".to_string(), vec![], spec, ScalarType::Int, o()).unwrap();
    assert_eq!(w.phase, Phase::Win);
    assert_eq!(
        erase_expr(w).unwrap(),
        Expr::Win(
            "ROW_NUMBER()".to_string(),
            vec![],
            Box::new(crate::ir::WinSpec {
                partition: vec![Expr::Col(Side::Single, "a".into())],
                order: vec![(Expr::Col(Side::Single, "b".into()), true)],
                frame: Some(frame),
            })
        )
    );
}

#[test]
fn a_checked_node_carries_its_origin() {
    let q = CheckedQuery::where_(table(), bool_col("a"), o()).unwrap();
    assert_eq!(q.origin, o());
    assert_eq!(q.node.exprs()[0].origin, o());
}

#[test]
fn phases_are_the_mix_of_the_arguments() {
    // Row arguments give a row-phase template.
    let r = CheckedExpr::template("$1".to_string(), vec![int_col("a")], ScalarType::Int, o()).unwrap();
    assert_eq!(r.phase, Phase::Row);
    // Constants alone are constant.
    let c = CheckedExpr::template(
        "1".to_string(),
        vec![CheckedExpr::lit(Lit::Int(1), o())],
        ScalarType::Int,
        o(),
    )
    .unwrap();
    assert_eq!(c.phase, Phase::Const);
    // A constant template over an aggregate is an aggregate, which is how
    // `inc : agg (expr r int) -> expr r int = sql "$1 + 1"` stays one.
    let a = CheckedExpr::agg_template("SUM($1)".to_string(), vec![int_col("a")], ScalarType::Int, o())
        .unwrap();
    let mix = CheckedExpr::template("$1 + 1".to_string(), vec![a], ScalarType::Int, o()).unwrap();
    assert_eq!(mix.phase, Phase::Agg);
}

#[test]
fn group_is_aggregate_phase_with_the_keys_type() {
    let g = CheckedExpr::group(col("b", ScalarType::String), o()).unwrap();
    assert_eq!(g.phase, Phase::Agg);
    assert_eq!(g.ty, ScalarType::String);
}

#[test]
fn windows_and_aggregates_nest_and_mix_only_through_their_own_constructor() {
    let a = CheckedExpr::agg_template("SUM($1)".to_string(), vec![int_col("a")], ScalarType::Int, o())
        .unwrap();
    let w = CheckedExpr::win_template(
        "R()".to_string(),
        vec![],
        window_spec(vec![], vec![], None).unwrap(),
        ScalarType::Int,
        o(),
    )
    .unwrap();
    // Aggregate mixed with a row column: a clash, not a phase.
    assert!(CheckedExpr::template("$1 + $2".to_string(), vec![a.clone(), int_col("b")], ScalarType::Int, o())
        .is_err());
    // Aggregate mixed with a window: its own message.
    let e = CheckedExpr::template("$1 + $2".to_string(), vec![a, w], ScalarType::Int, o()).unwrap_err();
    assert!(e.message.contains("window"), "{e}");
}

// ── negative: one test per rule ────────────────────────────────────────────

#[test]
fn where_rejects_an_aggregate_predicate() {
    let pred = CheckedExpr::agg_template("BOOL_AND($1)".to_string(), vec![bool_col("a")], ScalarType::Bool, o())
        .unwrap();
    let e = CheckedQuery::where_(table(), pred, o()).unwrap_err();
    assert!(e.message.contains("`where` cannot filter on an aggregate"), "{e}");
    assert_eq!(e.origin, Some(o()));
}

#[test]
fn where_rejects_a_window_predicate() {
    let pred = CheckedExpr::win_template(
        "R()".to_string(),
        vec![],
        window_spec(vec![], vec![], None).unwrap(),
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let e = CheckedQuery::where_(table(), pred, o()).unwrap_err();
    assert!(e.message.contains("window function"), "{e}");
}

#[test]
fn where_rejects_a_non_bool_predicate() {
    let e = CheckedQuery::where_(table(), int_col("a"), o()).unwrap_err();
    assert!(e.message.contains("must be bool"), "{e}");
}

#[test]
fn where_rejects_an_unknown_column() {
    let e = CheckedQuery::where_(table(), bool_col("nope"), o()).unwrap_err();
    assert!(e.message.contains("no column `nope` in the input of `where`"), "{e}");
    assert!(e.message.contains("available: a, b"), "{e}");
}

#[test]
fn select_rejects_a_sided_column_reference() {
    // `.<a` is a join-predicate form; outside a join it is `rules::JOIN_ONLY`.
    let e = CheckedQuery::select(
        table(),
        vec![(
            "x".to_string(),
            CheckedExpr::column(Side::Left, "a", ScalarType::Int, o()),
        )],
        o(),
    )
    .unwrap_err();
    assert!(e.message.contains(rules::JOIN_ONLY), "{e}");
}

#[test]
fn select_rejects_an_unknown_column() {
    let e = CheckedQuery::select(table(), vec![("x".into(), int_col("zz"))], o()).unwrap_err();
    assert!(e.message.contains("field `x`: no column `zz`"), "{e}");
}

#[test]
fn select_rejects_an_aggregate_field() {
    let a = CheckedExpr::agg_template("SUM($1)".to_string(), vec![int_col("a")], ScalarType::Int, o())
        .unwrap();
    let e = CheckedQuery::select(table(), vec![("x".into(), a)], o()).unwrap_err();
    assert!(e.message.contains("aggregates belong in `agg`"), "{e}");
}

#[test]
fn agg_rejects_a_row_phase_field_that_is_not_grouped() {
    let e = CheckedQuery::agg(table(), vec![("x".into(), int_col("a"))], o()).unwrap_err();
    assert!(e.message.contains("not grouped"), "{e}");
}

#[test]
fn agg_rejects_a_window_field() {
    let w = CheckedExpr::win_template(
        "R()".to_string(),
        vec![],
        window_spec(vec![], vec![], None).unwrap(),
        ScalarType::Int,
        o(),
    )
    .unwrap();
    let e = CheckedQuery::agg(table(), vec![("x".into(), w)], o()).unwrap_err();
    assert!(e.message.contains("window function"), "{e}");
}

#[test]
fn order_rejects_an_aggregate_key() {
    let a = CheckedExpr::agg_template("COUNT(*)".to_string(), vec![], ScalarType::Int, o()).unwrap();
    let e = CheckedQuery::order(table(), vec![(a, true)], o()).unwrap_err();
    assert!(e.message.contains("plain column expressions"), "{e}");
}

#[test]
fn join_rejects_a_plain_column_in_the_predicate() {
    let on = bool_col("a");
    let e = CheckedQuery::join(JoinKind::Inner, table(), table(), on, o()).unwrap_err();
    assert_eq!(e.message, rules::needs_side("a"));
}

#[test]
fn join_rejects_a_column_absent_from_its_own_side() {
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Right, "nope", ScalarType::Bool, o())],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let e = CheckedQuery::join(JoinKind::Inner, table(), table(), on, o()).unwrap_err();
    assert!(e.message.contains("the right join input has no column `nope`"), "{e}");
}

#[test]
fn join_rejects_a_non_bool_predicate() {
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Left, "a", ScalarType::Int, o())],
        ScalarType::Int,
        o(),
    )
    .unwrap();
    let e = CheckedQuery::join(JoinKind::Inner, table(), table(), on, o()).unwrap_err();
    assert!(e.message.contains("must be bool"), "{e}");
}

#[test]
fn left_join_makes_exactly_the_right_side_nullable() {
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Left, "a", ScalarType::Bool, o())],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let q = CheckedQuery::join(JoinKind::Left, table(), table(), on, o()).unwrap();
    // The left columns keep their types; the right side's become `maybe`,
    // exactly once (the left row already owns the names, so the right's
    // collisions do not appear twice).
    assert_eq!(q.row.get("a"), Some(&ScalarType::Int));
    assert_eq!(q.row.get("b"), Some(&ScalarType::String));
    assert!(!q.row.get("a").unwrap().is_maybe());
}

#[test]
fn left_join_nullability_applies_to_right_only_columns() {
    let left = CheckedQuery::table(
        "s",
        "l",
        Some(RowType::new(vec![("id".into(), ScalarType::Int)])),
        o(),
    )
    .unwrap();
    let right = CheckedQuery::table(
        "s",
        "r",
        Some(RowType::new(vec![
            ("id".into(), ScalarType::Int),
            ("extra".into(), ScalarType::String),
        ])),
        o(),
    )
    .unwrap();
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Left, "id", ScalarType::Bool, o())],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let q = CheckedQuery::join(JoinKind::Left, left, right, on, o()).unwrap();
    // `id` comes from the left and stays non-null; `extra` is the right's and
    // becomes nullable exactly once.
    assert_eq!(q.row.get("id"), Some(&ScalarType::Int));
    assert_eq!(
        q.row.get("extra"),
        Some(&ScalarType::Maybe(Box::new(ScalarType::String)))
    );
    assert_eq!(q.row.names(), vec!["id".to_string(), "extra".to_string()]);
}

#[test]
fn full_join_makes_both_sides_nullable_once() {
    let left = CheckedQuery::table(
        "s",
        "l",
        Some(RowType::new(vec![("id".into(), ScalarType::Int)])),
        o(),
    )
    .unwrap();
    let right = CheckedQuery::table(
        "s",
        "r",
        Some(RowType::new(vec![(
            "n".to_string(),
            ScalarType::Maybe(Box::new(ScalarType::String)),
        )])),
        o(),
    )
    .unwrap();
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Left, "id", ScalarType::Bool, o())],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    let q = CheckedQuery::join(JoinKind::Full, left, right, on, o()).unwrap();
    assert_eq!(
        q.row.get("id"),
        Some(&ScalarType::Maybe(Box::new(ScalarType::Int)))
    );
    // Already nullable: `maybe (maybe t) = maybe t`.
    assert_eq!(
        q.row.get("n"),
        Some(&ScalarType::Maybe(Box::new(ScalarType::String)))
    );
}

#[test]
fn semi_and_anti_joins_keep_the_left_row() {
    let on = CheckedExpr::template(
        "$1".to_string(),
        vec![CheckedExpr::column(Side::Left, "a", ScalarType::Bool, o())],
        ScalarType::Bool,
        o(),
    )
    .unwrap();
    for kind in [JoinKind::Semi, JoinKind::Anti] {
        let q = CheckedQuery::join(kind, table(), table(), on.clone(), o()).unwrap();
        assert_eq!(q.row, table().row, "{kind:?} must keep the left row");
    }
}

#[test]
fn set_rejects_a_row_mismatch() {
    let other = CheckedQuery::table(
        "s",
        "u",
        Some(RowType::new(vec![
            ("a".into(), ScalarType::Int),
            ("c".into(), ScalarType::String),
        ])),
        o(),
    )
    .unwrap();
    let e = CheckedQuery::set(SetKind::Union, table(), other, o()).unwrap_err();
    assert!(e.message.contains("must have the same columns"), "{e}");
}

#[test]
fn set_rejects_a_column_type_mismatch() {
    let other = CheckedQuery::table(
        "s",
        "u",
        Some(RowType::new(vec![
            ("a".into(), ScalarType::String),
            ("b".into(), ScalarType::String),
        ])),
        o(),
    )
    .unwrap();
    let e = CheckedQuery::set(SetKind::Union, table(), other, o()).unwrap_err();
    assert!(e.message.contains("same column types"), "{e}");
    assert!(e.message.contains("`a` is int on the left but string on the right"), "{e}");
}

#[test]
fn select_update_and_agg_reject_an_empty_field_list() {
    assert_eq!(
        CheckedQuery::select(table(), vec![], o()).unwrap_err().message,
        "`select` needs at least one field"
    );
    assert_eq!(
        CheckedQuery::update(table(), vec![], o()).unwrap_err().message,
        "`update` needs at least one field"
    );
    assert_eq!(
        CheckedQuery::agg(table(), vec![], o()).unwrap_err().message,
        "`agg` needs at least one field"
    );
}

#[test]
fn a_repeated_field_is_reported_per_stage_like_the_checker_does() {
    let dup = vec![("x".into(), int_col("a")), ("x".into(), int_col("b"))];
    assert_eq!(
        CheckedQuery::select(table(), dup.clone(), o())
            .unwrap_err()
            .message,
        "field `x` appears twice in `select`"
    );
    assert_eq!(
        CheckedQuery::update(table(), dup, o()).unwrap_err().message,
        "field `x` appears twice in `update`"
    );
}

// ── BUG #1 regression: `update` is right-wins, not left-wins ───────────────

#[test]
fn update_takes_the_new_type_of_an_overwritten_column() {
    // `t : query { id = int, n = string }`, `t & update { id = "hello" }`
    // types as `{ id = string, n = string }`. `RowType::merge` (the join law,
    // left-wins) would have kept `int` here.
    let q = CheckedQuery::update(
        table(),
        vec![("a".into(), col("b", ScalarType::String))],
        o(),
    )
    .unwrap();
    assert_eq!(q.row.get("a"), Some(&ScalarType::String));
    assert_eq!(q.row.get("b"), Some(&ScalarType::String));
    // Position is the input's, so `a` stays first.
    assert_eq!(q.row.names(), vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn update_appends_a_field_the_input_does_not_have() {
    let q = CheckedQuery::update(
        table(),
        vec![("fresh".into(), CheckedExpr::lit(Lit::Int(1), o()))],
        o(),
    )
    .unwrap();
    assert_eq!(
        q.row.names(),
        vec!["a".to_string(), "b".to_string(), "fresh".to_string()]
    );
    assert_eq!(q.row.get("fresh"), Some(&ScalarType::Int));
}

#[test]
fn update_keeps_positions_on_a_same_type_overwrite() {
    let q = CheckedQuery::update(table(), vec![("b".into(), int_col("a"))], o()).unwrap();
    assert_eq!(q.row.names(), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(q.row.get("b"), Some(&ScalarType::Int));
}

// ── the `erase`/`schema` invariant ─────────────────────────────────────────

#[test]
fn schema_of_the_erasure_equals_the_recorded_row() {
    // The invariant that ties the checked layer to the validator: a query that
    // exists erases to IR whose schema is the row it recorded.
    let q = CheckedQuery::select(
        CheckedQuery::where_(table(), bool_col("a"), o()).unwrap(),
        vec![("x".into(), int_col("a")), ("y".into(), col("b", ScalarType::String))],
        o(),
    )
    .unwrap();
    let expected: Vec<String> = q.row.columns().iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(crate::schema::schema(&q.clone().erase()).unwrap(), expected);
}

#[test]
fn schema_of_the_erasure_equals_the_recorded_row_for_every_small_stage() {
    let cases: Vec<CheckedQuery> = vec![
        table(),
        CheckedQuery::where_(table(), bool_col("a"), o()).unwrap(),
        CheckedQuery::select(table(), vec![("x".into(), int_col("a"))], o()).unwrap(),
        CheckedQuery::update(table(), vec![("a".into(), int_col("b"))], o()).unwrap(),
        CheckedQuery::omit(table(), "b", o()).unwrap(),
        CheckedQuery::prefix(table(), "u_", o()).unwrap(),
        CheckedQuery::order(table(), vec![(int_col("a"), true)], o()).unwrap(),
        CheckedQuery::limit(table(), 1, o()).unwrap(),
        CheckedQuery::distinct(table(), o()).unwrap(),
    ];
    for q in cases {
        let expected: Vec<String> = q.row.columns().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(
            crate::schema::schema(&q.clone().erase()).unwrap(),
            expected,
            "row and erased schema disagree for {q:?}"
        );
    }
}

#[test]
fn erase_checked_reports_both_the_rel_and_its_columns() {
    let (rel, cols) = erase_checked(table()).unwrap();
    assert_eq!(rel, base_rel());
    assert_eq!(cols, vec!["a".to_string(), "b".to_string()]);
}

// ── the `omit` invariant (D3/C) ────────────────────────────────────────────

#[test]
fn omit_rejects_a_key_the_input_does_not_have() {
    // `RowType::omit` is total, so before this rule the constructor accepted a
    // query `schema::schema` rejects — reachable through `pub` API alone.
    let e = CheckedQuery::omit(table(), "nope", o()).unwrap_err();
    assert_eq!(e.message, "no column `nope`; available: a, b");
    assert_eq!(e.origin, Some(o()));
}

#[test]
fn omit_and_schema_agree_on_a_missing_key() {
    // The checked constructor and the untyped validator reject the same
    // program, so the two layers state one rule.
    let missing = "nope";
    assert!(CheckedQuery::omit(table(), missing, o()).is_err());
    assert!(crate::schema::schema(&Rel::Omit(Box::new(base_rel()), missing.into())).is_err());
}

#[test]
fn omit_accepts_a_key_the_input_has() {
    let q = CheckedQuery::omit(table(), "a", o()).unwrap();
    assert_eq!(q.row.names(), vec!["b".to_string()]);
    assert_eq!(crate::schema::schema(&q.erase()).unwrap(), vec!["b".to_string()]);
}

// ── `RowType` laws, pinned where they are consumed ─────────────────────────

#[test]
fn merge_is_left_wins_and_overwrite_is_right_wins() {
    let left = RowType::new(vec![
        ("id".into(), ScalarType::Int),
        ("n".into(), ScalarType::String),
    ]);
    let right = RowType::new(vec![("id".into(), ScalarType::String)]);
    // The join law keeps the left type...
    assert_eq!(left.merge(&right).get("id"), Some(&ScalarType::Int));
    // ...and the overwrite law takes the right one, in the left's position.
    assert_eq!(left.overwrite(&right).get("id"), Some(&ScalarType::String));
    assert_eq!(left.overwrite(&right).names(), left.names());
}

// ── `CheckedProgram` ───────────────────────────────────────────────────────

#[test]
fn a_checked_module_carries_its_definitions_and_choices() {
    let mut m = CheckedModule::new(3, "<input>");
    m.types.insert(0, "query { a = int }".into());
    m.holes.insert(0, 1);
    m.choices.insert((0, 7, 0), Choice::Def(2, 4));
    m.defs.push(CheckedDef {
        name: "q".into(),
        def: 0,
        scheme: Some("query { a = int }".into()),
        scalar: None,
        row: Some(RowType::new(vec![("a".into(), ScalarType::Int)])),
        holes: 1,
        terms: vec![],
    });
    assert_eq!(m.index, 3);
    assert_eq!(m.choice(0, 7, 0), Some(Choice::Def(2, 4)));
    assert_eq!(m.def(0).unwrap().name, "q");
    assert_eq!(m.def(9), None);

    let p = CheckedProgram {
        modules: vec![m],
        diagnostics: vec![Diagnostic::new(
            3,
            Some(0),
            crate::workspace::Diag {
                path: "<input>".into(),
                line: 1,
                col: 1,
                message: "boom".into(),
                source: String::new(),
                width: 1,
            },
        )],
    };
    assert!(!p.is_ok());
    assert_eq!(p.module(3).unwrap().defs.len(), 1);
    assert_eq!(p.module(9), None);
}

// ── `from_rel` narrowing ───────────────────────────────────────────────────

#[test]
fn from_rel_keeps_a_known_table_type() {
    let rel = Rel::Table {
        schema: "s".into(),
        name: "t".into(),
        columns: Some(vec!["id".into()]),
    };
    let row = RowType::new(vec![("id".into(), ScalarType::Int)]);
    let q = from_rel(rel, row, None, o()).unwrap();
    // The caller's typed row must not be downgraded to `Unknown`.
    assert_eq!(q.row.get("id"), Some(&ScalarType::Int));
}

#[test]
fn from_rel_round_trips_the_shapes_a_rel_records() {
    let origin = Origin::new(1, Span { start: 4, end: 12 });
    let q = CheckedQuery::where_(table(), bool_col("a"), origin).unwrap();
    let rel = q.clone().erase();
    let back = from_rel(rel.clone(), q.row.clone(), Some(StageHint::Select), origin).unwrap();
    assert_eq!(back.clone().erase(), rel);
    assert_eq!(back.row, q.row);
}

#[test]
fn from_rel_refuses_to_guess_between_select_and_agg() {
    // `Rel::Select` and `Rel::Agg` are one variant, so a `Rel` cannot say
    // which it was; guessing is what a bridge must not do.
    let rel = Rel::Select(Box::new(base_rel()), vec![]);
    let e = from_rel(rel, RowType::new(vec![]), None, o()).unwrap_err();
    assert!(e.message.contains("does not say whether it was a `select` or an `agg`"), "{e}");
}

#[test]
fn from_rel_distinguishes_select_from_agg_with_a_hint() {
    let sel = Rel::Select(Box::new(base_rel()), vec![]);
    let q = from_rel(sel, RowType::new(vec![]), Some(StageHint::Select), o()).unwrap();
    assert!(matches!(q.node, CheckedQueryNode::Select { .. }));

    let agg = Rel::Agg(Box::new(base_rel()), vec![]);
    let q = from_rel(agg, RowType::new(vec![]), Some(StageHint::Agg), o()).unwrap();
    assert!(matches!(q.node, CheckedQueryNode::Agg { .. }));
}

#[test]
fn from_rel_refuses_a_non_invertible_rename() {
    // `prefix "u_"` on a column that does not start with `u_` cannot give back
    // the input row, so it is an error rather than the output row passed off
    // as the input.
    let rel = Rel::Prefix(
        Box::new(base_rel()),
        "u_".to_string(),
    );
    let row = RowType::new(vec![
        ("a".into(), ScalarType::Int),
        ("b".into(), ScalarType::String),
    ]);
    let e = from_rel(rel, row, None, o()).unwrap_err();
    assert!(e.message.contains("does not carry the `prefix \"u_\"` affix"), "{e}");
}

#[test]
fn from_rel_accepts_an_invertible_rename() {
    let inner = Rel::Table {
        schema: "s".into(),
        name: "t".into(),
        columns: Some(vec!["a".into()]),
    };
    let rel = Rel::Prefix(Box::new(inner), "u_".into());
    let row = RowType::new(vec![("u_a".into(), ScalarType::Int)]);
    let q = from_rel(rel, row, None, o()).unwrap();
    assert!(matches!(q.node, CheckedQueryNode::Rename { prefix: true, .. }));
}

// ── helpers ────────────────────────────────────────────────────────────────

#[test]
fn a_frame_rejects_impossible_bounds() {
    assert!(frame(crate::ir::Bound::Following(2), crate::ir::Bound::Following(1)).is_err());
    assert!(frame(crate::ir::Bound::CurrentRow, crate::ir::Bound::CurrentRow).is_ok());
}

#[test]
fn a_window_spec_checks_its_keys() {
    let a = CheckedExpr::agg_template("COUNT(*)".to_string(), vec![], ScalarType::Int, o()).unwrap();
    assert!(window_spec(vec![a], vec![], None).is_err());
}

#[test]
fn an_error_keeps_the_innermost_origin() {
    let e = Error::new("x")
        .at(Origin::new(1, span(5)))
        .at(Origin::new(2, span(9)));
    assert_eq!(e.origin, Some(Origin::new(1, span(5))));
}

#[test]
fn a_checked_query_reports_its_column_references() {
    let q = CheckedQuery::where_(table(), bool_col("a"), o()).unwrap();
    assert_eq!(q.node.columns(), vec![(Side::Single, "a".to_string())]);
    // A table has no expressions of its own.
    assert!(table().node.exprs().is_empty());
    assert!(table().node.input().is_none());
}

// ── CheckedProgram from a real source (the `check/` wiring) ────────────────

/// A source whose definitions exercise every shape `CheckedDef` records.
const SRC: &str = "users : query { id = int, name = string, age = int } = table \"public\" \"users\"\n\
                   young = users & where (.age > 1)\n\
                   named = young & select { n = .name, a = .age }\n\
                   plus1 = x => x + 1\n";

fn program() -> CheckedProgram {
    let ws = crate::workspace::Workspace::from_source(SRC);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    CheckedProgram::of(&ws)
}

fn root_def(p: &CheckedProgram, name: &str) -> CheckedDef {
    let ws = crate::workspace::Workspace::from_source(SRC);
    let root = ws.root;
    p.module(root)
        .expect("the root module is checked")
        .defs
        .iter()
        .find(|d| d.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no definition `{name}`"))
}

#[test]
fn a_checked_program_gathers_every_module() {
    let ws = crate::workspace::Workspace::from_source(SRC);
    let p = CheckedProgram::of(&ws);
    assert_eq!(p.modules.len(), ws.modules.len());
    for m in &p.modules {
        assert_eq!(m.index, m.index);
    }
    assert!(p.is_ok(), "{:?}", p.diagnostics);
    // The prelude is module 0 and the root is last.
    assert!(p.module(ws.root).is_some());
}

#[test]
fn a_checked_def_carries_a_typed_row_from_the_checker() {
    // This is the wiring: `ScalarType` and `RowType` come from `TypeCheck`,
    // not from re-deriving anything, so the two cannot disagree.
    let p = program();
    let d = root_def(&p, "named");
    let row = d.row().expect("`named` is a query over a closed row");
    assert_eq!(row.names(), vec!["n".to_string(), "a".to_string()]);
    assert_eq!(row.get("n"), Some(&ScalarType::String));
    assert_eq!(row.get("a"), Some(&ScalarType::Int));
    assert_eq!(d.row_names(), vec!["n".to_string(), "a".to_string()]);
}

#[test]
fn a_checked_def_carries_its_scalar_scheme_and_its_printed_one() {
    let p = program();
    let d = root_def(&p, "young");
    // The printed form is what a user reads...
    assert!(d.scheme.as_deref().unwrap_or("").contains("query"));
    // ...and the typed form is what a phase reads.
    assert!(d.row().is_some());
}

#[test]
fn a_function_definition_has_no_row() {
    // `plus1 = x => x + 1` is a function: `None`, not an empty row. The two
    // must not be collapsed.
    let p = program();
    let d = root_def(&p, "plus1");
    assert!(d.row().is_none(), "a function has no output row");
    assert!(d.row_names().is_empty());
}

#[test]
fn a_checked_program_reports_the_same_diagnostics_as_the_checker() {
    let src = "t : query { a = int } = table \"s\" \"t\"\nq = t & where .a\n";
    let ws = crate::workspace::Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    let p = CheckedProgram::of(&ws);
    // Same number of type errors, and each carries its module and definition.
    assert_eq!(p.diagnostics.len(), tc.errors.len() + ws.diags.len());
    assert!(!p.is_ok());
    for d in &p.diagnostics {
        assert!(d.module < ws.modules.len());
        assert_eq!(d.def.is_some(), tc.errors.iter().any(|e| e.def == d.def.unwrap_or(usize::MAX)));
    }
}

#[test]
fn checked_program_from_type_check_reuses_a_check_already_run() {
    let ws = crate::workspace::Workspace::from_source(SRC);
    let tc = crate::check::check(&ws);
    let p = CheckedProgram::from_type_check(&ws, &tc, std::collections::HashMap::new());
    assert_eq!(p.modules.len(), ws.modules.len());
    assert!(p.is_ok());
    // No bodies were supplied, so every definition's term list is empty
    // rather than invented.
    for m in &p.modules {
        for d in &m.defs {
            assert!(d.terms.is_empty());
        }
    }
}
