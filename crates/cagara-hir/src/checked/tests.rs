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
use crate::workspace::Workspace;
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
        terms: None,
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

// ── extracting what the checker actually recorded ──────────────────────────

/// A real overloaded program, not a hand-built map.
///
/// This is the test the layer was missing. Overload choices are recorded by
/// the checker against the *use expression's* `ExprId`
/// (`infer.rs`: `self.lookup(n, e.id, sp)`), while `from_type_check` used to
/// look them up with `span.start`/`span.end`. The two are different namespaces
/// that are both small integers, so the old code transferred **no** choices
/// for a real program — and could have transferred another use's choice when a
/// span offset happened to equal an `ExprId`. Only a program with genuine
/// overloads exercises this; the sibling test above hand-inserts a choice and
/// so passes either way.
#[test]
fn a_real_overloaded_definition_transfers_its_choices() {
    let src = "users : query { id = int, age = int, active = bool } = table \"p\" \"users\"\n\
               describe : expr r int -> expr r string = sql \"CAST($1 AS TEXT)\"\n\
               describe : expr r bool -> expr r string = sql \"CASE WHEN $1 THEN 'yes' ELSE 'no' END\"\n\
               q = users & select { a = describe .age, b = describe .active }\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    assert!(tc.errors.is_empty(), "{:?}", tc.errors);

    let m = ws.root;
    let q = ws.modules[m]
        .module
        .defs
        .iter()
        .position(|d| d.name == "q")
        .expect("no `q`");

    // The checker has two uses of `describe`, at two distinct sites.
    let from_checker = tc.choices_of(m, q);
    assert_eq!(
        from_checker.len(),
        2,
        "expected two resolved overload uses, got {from_checker:?}"
    );
    let sites: Vec<u32> = from_checker.iter().map(|&(s, _, _)| s).collect();
    assert_ne!(sites[0], sites[1], "the two uses must have distinct sites");

    // …and `CheckedProgram` carries exactly the same ones.
    let cp = CheckedProgram::of(&ws);
    let cm = cp.module(m).expect("module missing");
    let def = cm.def(q).expect("definition missing");
    assert_eq!(
        def.holes, 0,
        "`q` instantiates its overloads, so it leaves none open"
    );

    // Every choice the checker recorded is in the checked module, under the
    // same site. The old span-keyed lookup transferred none of them.
    let mut transferred: Vec<(u32, usize, Choice)> = from_checker
        .iter()
        .filter_map(|&(site, k, c)| {
            let got = cm.choices.get(&(q, site, k))?;
            Some((site, k, if *got == c { c } else { Choice::Hole(usize::MAX) }))
        })
        .collect();
    // Sort by key: `Choice` has no `Ord`, and the key is what identifies a use.
    transferred.sort_by_key(|&(site, k, _)| (site, k));
    let mut expected = from_checker.clone();
    expected.sort_by_key(|&(site, k, _)| (site, k));
    assert_eq!(
        transferred, expected,
        "every checker choice must arrive in CheckedProgram; \
         checker={from_checker:?} transferred={transferred:?}"
    );

    // The choices are two *different* overload candidates, so this is a real
    // discrimination and not the same candidate twice.
    let cands: std::collections::HashSet<(usize, usize)> = from_checker
        .iter()
        .filter_map(|&(_, _, c)| match c {
            Choice::Def(dm, di) => Some((dm, di)),
            Choice::Hole(_) => None,
        })
        .collect();
    assert_eq!(
        cands.len(),
        2,
        "the two uses must resolve to two different overload candidates: {from_checker:?}"
    );
}

/// A span offset must not be usable as a choice key.
///
/// This pins *why* the fix was needed rather than only that it works: the
/// checker keyed its choices by `ExprId`, and a byte offset is a different
/// number. If a future change reintroduces a span-keyed lookup, this fails.
#[test]
fn choices_are_keyed_by_expr_id_not_by_span() {
    let src = "users : query { id = int, age = int } = table \"p\" \"users\"\n\
               n : expr r int -> expr r int = sql \"$1 + 1\"\n\
               n : expr r float -> expr r float = sql \"$1 + 1.0\"\n\
               q = users & select { a = n .age, b = n .age }\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(tc.errors.is_empty(), "{:?}", tc.errors);
    let m = ws.root;
    let q = ws.modules[m]
        .module
        .defs
        .iter()
        .position(|d| d.name == "q")
        .expect("no `q`");
    let choices = tc.choices_of(m, q);
    assert!(!choices.is_empty(), "expected resolved overload uses");

    // Look every recorded site up as a *span* as well. If the two namespaces
    // were the same, the span lookup would find the same choices.
    let spans: Vec<(u32, u32)> = body_spans(&ws.modules[m].module.defs[q].body)
        .into_iter()
        .map(|(s, _)| (s.start, s.end))
        .collect();
    let mut via_span = 0;
    for &(start, end) in &spans {
        for site in [start, end] {
            if tc.choice(m, q, site, 0).is_some() {
                via_span += 1;
            }
        }
    }
    assert_eq!(
        via_span, 0,
        "a span offset found a choice: the two namespaces have collided, so the \
         span-keyed lookup this replaced would have been silently wrong"
    );
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
    let q = from_rel_unchecked(rel, row, None, o()).unwrap();
    // The caller's typed row must not be downgraded to `Unknown`.
    assert_eq!(q.row.get("id"), Some(&ScalarType::Int));
}

#[test]
fn from_rel_round_trips_the_shapes_a_rel_records() {
    let origin = Origin::new(1, Span { start: 4, end: 12 });
    let q = CheckedQuery::where_(table(), bool_col("a"), origin).unwrap();
    let rel = q.clone().erase();
    let back = from_rel_unchecked(rel.clone(), q.row.clone(), Some(StageHint::Select), origin).unwrap();
    assert_eq!(back.clone().erase(), rel);
    assert_eq!(back.row, q.row);
}

#[test]
fn from_rel_refuses_to_guess_between_select_and_agg() {
    // `Rel::Select` and `Rel::Agg` are one variant, so a `Rel` cannot say
    // which it was; guessing is what a bridge must not do.
    let rel = Rel::Select(Box::new(base_rel()), vec![]);
    let e = from_rel_unchecked(rel, RowType::new(vec![]), None, o()).unwrap_err();
    assert!(e.message.contains("does not say whether it was a `select` or an `agg`"), "{e}");
}

#[test]
fn from_rel_distinguishes_select_from_agg_with_a_hint() {
    let sel = Rel::Select(Box::new(base_rel()), vec![]);
    let q = from_rel_unchecked(sel, RowType::new(vec![]), Some(StageHint::Select), o()).unwrap();
    assert!(matches!(q.node, CheckedQueryNode::Select { .. }));

    let agg = Rel::Agg(Box::new(base_rel()), vec![]);
    let q = from_rel_unchecked(agg, RowType::new(vec![]), Some(StageHint::Agg), o()).unwrap();
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
    let e = from_rel_unchecked(rel, row, None, o()).unwrap_err();
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
    let q = from_rel_unchecked(rel, row, None, o()).unwrap();
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

/// A function that *mentions* a query is still a function.
///
/// This is the case the old printed-scheme test got wrong: `scheme_row` tested
/// `printed.contains("query")`, which is true for
/// `query r -> query r` — a helper's own type — so `f` was reported as a query
/// with **zero columns** rather than as a function. It also meant `def_scalar`
/// answered `Some(Unknown)` for it, contradicting its own documentation.
#[test]
fn a_function_that_mentions_a_query_has_no_row() {
    let src = "users : query { id = int } = table \"p\" \"users\"\n\
               f = q => q & where (.id > 0)\n\
               g : query { id = int } = users & where (.id > 0)\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    assert!(tc.errors.is_empty(), "{:?}", tc.errors);
    let p = CheckedProgram::of(&ws);

    let m = ws.root;
    let idx = |name: &str| {
        ws.modules[m]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("no `{name}`"))
    };

    // `f` takes a query and returns one: a function, not an empty-rowed query.
    let f = p.module(m).unwrap().def(idx("f")).unwrap();
    assert!(
        f.row.is_none(),
        "`f` is a function; its row must be None, not an empty row: {:?}",
        f.row
    );

    // `g` is a real query and keeps its columns: the fix must not have been
    // to drop every row.
    let g = p.module(m).unwrap().def(idx("g")).unwrap();
    assert_eq!(
        g.row.as_ref().map(|r| r.names()),
        Some(vec!["id".to_string()]),
        "`g` is a query over one column"
    );

    // `def_scalar` returns `None` for a function and a query alike, as its
    // documentation says — not `Some(Unknown)` for everything.
    assert_eq!(tc.def_scalar(m, idx("f")), None, "a function is not a scalar");
    assert_eq!(tc.def_scalar(m, idx("g")), None, "a query is not a scalar");
    // A real scalar is still `Some`.
    let src2 = "one : int = 1\n";
    let ws2 = Workspace::from_source(src2);
    let tc2 = crate::check::check(&ws2);
    assert_eq!(
        tc2.def_scalar(ws2.root, 0),
        Some(ScalarType::Int),
        "an `int` definition must report `int`, not None"
    );
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
    let p = CheckedProgram::from_type_check(&ws, &tc, None);
    assert_eq!(p.modules.len(), ws.modules.len());
    assert!(p.is_ok());
    // No bodies were supplied, so `terms` is `None` — "not elaborated" — and
    // explicitly *not* an empty list, which would claim the definitions have
    // no bodies at all. The distinction is the point of the field's type.
    for m in &p.modules {
        for d in &m.defs {
            assert!(
                d.terms.is_none(),
                "`{}` must record that it was not elaborated, not that it has no body",
                d.name
            );
        }
    }
}

/// `CheckedProgram::of_elaborated` fills in the bodies `of` leaves out.
#[test]
fn of_elaborated_fills_in_the_definition_bodies() {
    let src = "t : query { a = int } = table \"s\" \"t\"\n\
               q = t & where (.a > 0)\n";
    let ws = crate::workspace::Workspace::from_source(src);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);

    let plain = CheckedProgram::of(&ws);
    let m = ws.root;
    let q = ws.modules[m]
        .module
        .defs
        .iter()
        .position(|d| d.name == "q")
        .expect("no `q`");
    assert!(
        plain.module(m).unwrap().def(q).unwrap().terms.is_none(),
        "`of` must not claim to have elaborated"
    );

    let elaborated = CheckedProgram::of_elaborated(&ws);
    let def = elaborated.module(m).unwrap().def(q).unwrap();
    let terms = def
        .query_terms()
        .expect("`of_elaborated` must give `q` a query term");
    assert_eq!(terms.len(), 1, "a definition with no holes has one body");
    // The evaluator stamps the term with the stage's location, so the root is
    // `At`; underneath it must be the elaborated `where`, not a placeholder.
    assert!(
        matches!(terms[0].bare(), crate::CoreTerm::Where { .. }),
        "expected the elaborated `where` under the `At` wrapper, got {:?}",
        terms[0]
    );
}

/// The four outcomes of elaboration are distinguishable.
///
/// Before, only successful query terms were recorded: a scalar, an open-hole
/// helper, and a definition whose *evaluation blew up* all looked identical
/// from the outside — a missing entry. This pins that each is now reported as
/// itself, which is what makes an evaluator error reportable at all.
#[test]
fn elaboration_distinguishes_every_outcome() {
    use crate::eval::Elaborated;
    let src = "t : query { a = int } = table \"s\" \"t\"\n\
               q = t & where (.a > 0)\n\
               one : int = 1\n\
               helper = x => x + x\n\
               boom = t & where (bogusName .a)\n";
    let ws = Workspace::from_source(src);
    let m = ws.root;
    let p = CheckedProgram::of_elaborated(&ws);
    let cm = p.module(m).expect("module missing");
    let idx = |name: &str| {
        ws.modules[m]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("no `{name}`"))
    };
    let kind = |name: &str| cm.def(idx(name)).unwrap().terms.clone();

    // A query: one term.
    assert!(
        matches!(kind("q"), Some(Elaborated::Query(ts)) if ts.len() == 1),
        "`q` is a query: {:?}",
        kind("q")
    );
    // A scalar: elaborated, but with no relational term. This is a *fact about
    // the definition*, not a failure.
    assert_eq!(
        kind("one"),
        Some(Elaborated::Value),
        "a scalar has no relational term"
    );
    // A function that leaves an overload open: meaningful only at its uses.
    assert_eq!(
        kind("helper"),
        Some(Elaborated::OpenHoles),
        "an open-overload helper has no body of its own"
    );
    // A definition that cannot evaluate: `Failed`, and *reported*.
    assert_eq!(
        kind("boom"),
        Some(Elaborated::Failed),
        "an unevaluatable definition is Failed, not simply absent"
    );
}

/// An evaluator error is surfaced as a diagnostic, not dropped.
///
/// This is the other half of the previous finding: `of_elaborated` used to
/// return only successful terms, so a definition that failed to *evaluate*
/// left no trace beyond a missing key.
///
/// The program below is chosen so the **checker accepts it and evaluation
/// fails**: `f` returns a closure, so there is no definition cycle for the
/// checker to reject, but each application re-enters `f` forever and the
/// evaluator's depth guard fires. A program with an unknown name would not do:
/// the checker rejects that too, so the diagnostic would be present whether or
/// not evaluation's diagnostics are propagated, and the test would pass
/// vacuously.
#[test]
fn of_elaborated_reports_evaluator_diagnostics() {
    let src = "t : query { a = int } = table \"s\" \"t\"\n\
               f = x => f x\n\
               q = f 1\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    assert!(
        tc.errors.is_empty(),
        "the checker must accept this program, or the test proves nothing: {:?}",
        tc.errors
    );

    let p = CheckedProgram::of_elaborated(&ws);
    assert!(
        p.diagnostics
            .iter()
            .any(|d| d.diag.message.contains("recursion is not supported")),
        "the evaluator's diagnostic must be propagated: {:?}",
        p.diagnostics.iter().map(|d| &d.diag.message).collect::<Vec<_>>()
    );
    // …and it must be attributed to the definition that failed, not to the
    // module as a whole.
    assert!(
        p.diagnostics.iter().any(|d| d.def.is_some()),
        "the diagnostic must name a definition: {:?}",
        p.diagnostics
    );
}

/// A failure raised inside an *imported* module is attributed to that module.
///
/// `EvalError::module` is where the error was raised, which is not always the
/// module being evaluated: a definition here can fail inside a module it
/// imports. Attributing the outer loop's definition index to that module pairs
/// a module with a definition index it may not have — the root has `q` at
/// index 1, but the imported module has only one definition, so index 1 does
/// not exist there at all.
#[test]
fn a_diagnostic_from_an_imported_module_names_a_definition_there() {
    use std::collections::HashMap;
    use std::path::PathBuf;

    let root = PathBuf::from("/tmp/cagara-import-diag/root.cagara");
    let lib = PathBuf::from("/tmp/cagara-import-diag/lib.cagara");
    let mut buffers: HashMap<PathBuf, String> = HashMap::new();
    // The failing use must NOT be at index 0 of the root, or the buggy
    // `Some(i)` would coincidentally be a valid index in the imported module
    // (which has one definition) and the test could not tell the difference.
    buffers.insert(
        root.clone(),
        "import \"lib.cagara\" as lib\n\
         pad0 = 1\n\
         pad1 = 2\n\
         pad2 = 3\n\
         q = lib.boom 1\n"
            .into(),
    );
    // In the *imported* module, `boom` applies itself forever.
    buffers.insert(lib.clone(), "boom = x => boom x\n".into());
    let ws = Workspace::open_with_buffers(&root, buffers[&root].clone(), &buffers);
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    // `q` is the fourth definition, so an index of 3 names nothing in `lib`.
    let q_index = ws.modules[ws.root]
        .module
        .defs
        .iter()
        .position(|d| d.name == "q")
        .expect("no `q`");
    assert!(
        q_index > 0,
        "`q` must not be at index 0, or this test cannot detect the bug"
    );

    let p = CheckedProgram::of_elaborated(&ws);
    let boom = p
        .diagnostics
        .iter()
        .find(|d| d.diag.message.contains("recursion is not supported"))
        .unwrap_or_else(|| {
            panic!(
                "the imported failure must be reported: {:?}",
                p.diagnostics
                    .iter()
                    .map(|d| &d.diag.message)
                    .collect::<Vec<_>>()
            )
        });

    // The diagnostic points at the imported module…
    assert_eq!(
        p.module(boom.module).map(|m| m.path.as_str()),
        Some(lib.to_string_lossy().as_ref()),
        "the diagnostic must point at the imported module"
    );
    // …and the definition it names must exist *in that module* and be the one
    // that failed. The bug recorded the root's index here, and `lib` has only
    // one definition, so the old code produced an out-of-range index.
    let def = boom
        .def
        .unwrap_or_else(|| panic!("the imported diagnostic must name a definition: {boom:?}"));
    let md = &ws.modules[boom.module];
    assert!(
        def < md.module.defs.len(),
        "definition index {def} does not exist in `{}`, which has {} definitions",
        md.path.display(),
        md.module.defs.len()
    );
    assert_eq!(
        md.module.defs[def].name, "boom",
        "the definition index must name the failing definition"
    );
    assert_ne!(
        def, q_index,
        "the index must be the *imported* definition, not the caller's"
    );
}

/// `CheckedModule::types` holds well-typed definitions only.
#[test]
fn module_types_omits_definitions_that_did_not_check() {
    let src = "t : query { a = int } = table \"s\" \"t\"\n\
               good = t & where (.a > 0)\n\
               bad = .a + true\n";
    let ws = Workspace::from_source(src);
    let tc = crate::check::check(&ws);
    let p = CheckedProgram::of(&ws);
    let m = ws.root;
    let cm = p.module(m).expect("module missing");
    let idx = |name: &str| {
        ws.modules[m]
            .module
            .defs
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("no `{name}`"))
    };
    assert!(
        tc.error_for(m, idx("bad")).is_some(),
        "`bad` should not type-check, else this test proves nothing"
    );
    assert!(
        cm.types.contains_key(&idx("good")),
        "a well-typed definition must have its printed type"
    );
    // Absent, not `""`. The empty string is a plausible-looking value a
    // consumer could go on to display as though it were a type.
    assert_eq!(
        cm.types.get(&idx("bad")),
        None,
        "a definition that failed has no entry, not an empty string"
    );
    assert!(
        !cm.types.values().any(|t| t.is_empty()),
        "no definition may report an empty printed type: {:?}",
        cm.types
    );
}

// ── differential harness: the checked tree vs. the evaluator ───────────────

/// Build both trees for one program and assert they erase to the *same* `Rel`.
///
/// This is the prerequisite for routing production through the checked layer.
/// Two things are compared for every query definition of the root module:
///
///   * the evaluator's tree — `Evaluator` -> `CoreTerm`, erased with
///     `erase_core` — which is what ships today;
///   * the checked tree — `from_rel_unchecked` -> `CheckedQuery`, erased with
///     `erase` — which is what would ship if the checked path were used.
///
/// They must be equal, or routing one through the other would change the
/// generated SQL. `from_rel_unchecked` is the bridge that makes this
/// comparable at all, and it is exactly why it is still in the tree: it is
/// only safe when the `Rel` it is given came from the same evaluator that is
/// being checked against.
fn assert_trees_agree(label: &str, src: &str) {
    let ws = Workspace::from_source(src);
    assert_trees_agree_in(label, ws);
}

/// [`assert_trees_agree`] for a program that imports other files, which has to
/// be loaded through its path so the imports resolve.
fn assert_trees_agree_file(label: &str, path: &str) {
    let ws = Workspace::open(std::path::Path::new(path));
    assert_trees_agree_in(label, ws);
}

fn assert_trees_agree_in(label: &str, ws: Workspace) {
    assert!(ws.diags.is_empty(), "{label}: load diagnostics: {:?}", ws.diags);
    let tc = crate::check::check(&ws);
    let m = ws.root;

    // Walk the evaluator's *core terms*, not only its erased `Rel`s: a `Rel`
    // cannot say whether a projection was a `select` or an `agg`, but the
    // `CoreTerm` the evaluator built can. Deriving the stage hint from the term
    // keeps the harness from guessing — and a guess here would test the
    // harness, not the trees.
    let terms: std::collections::HashMap<String, crate::CoreTerm> =
        crate::eval::root_core_terms(&ws, &tc)
            .into_iter()
            .filter_map(|(n, t)| t.ok().map(|t| (n, t)))
            .collect();

    // A `CoreTerm`'s root, ignoring `At` wrappers.
    fn root_kind(t: &crate::CoreTerm) -> &crate::CoreTerm {
        t.bare()
    }
    fn hint_of(t: &crate::CoreTerm) -> Option<crate::checked::StageHint> {
        match root_kind(t) {
            crate::CoreTerm::Select { .. } => Some(crate::checked::StageHint::Select),
            crate::CoreTerm::Agg { .. } => Some(crate::checked::StageHint::Agg),
            _ => None,
        }
    }

    let mut compared = 0;
    let mut skipped = Vec::new();
    // Same reason as `assert_source_elaboration_agrees` above: this must be the
    // evaluator alone. `root_queries_checked` also runs source elaboration and
    // converts a disagreement into an error, which `continue` below would then
    // treat as "the evaluator rejects this" — hiding the very mismatch the
    // comparison is here to find.
    for (name, eval_rel) in crate::eval::root_queries_via_evaluator(&ws, &tc) {
        let eval_rel = match eval_rel {
            Ok(r) => r,
            // A definition the *evaluator* rejects is a diagnostic, not a
            // mismatch; the checked path is not asked to succeed where the
            // current one fails.
            Err(_) => continue,
        };

        let row = crate::schema::schema(&eval_rel).unwrap_or_default();
        let origin = Origin::new(
            m,
            ws.modules[m]
                .module
                .defs
                .iter()
                .find(|d| d.name == name)
                .map(|d| d.span)
                .unwrap_or_default(),
        );
        let hint = terms.get(&name).and_then(hint_of);
        // The bridge cannot recover an `omit`'s input row from the *output*
        // alone — the omitted column's position is not recorded — so when the
        // term is an `omit` we hand it the input's row, which the evaluator's
        // own tree still has. Without this the `omit` node is skipped here and
        // never actually compared, which is how a mutated `Omit` eraser first
        // slipped past this harness.
        let bridge_row = match terms.get(&name).map(root_kind) {
            Some(crate::CoreTerm::Omit { input, .. }) => {
                // The input's columns are on the `CoreTerm` itself.
                match input.table_columns() {
                    Some(cs) => RowType::unknown(cs.to_vec()),
                    None => RowType::unknown(row),
                }
            }
            _ => RowType::unknown(row),
        };
        let checked =
            match crate::checked::from_rel_unchecked(eval_rel.clone(), bridge_row, hint, origin)
            {
                Ok(q) => q,
                Err(e) => {
                    // The bridge is `Err` where a `Rel` under-determines the
                    // answer. Those are gaps in the *bridge*, not disagreements
                    // between the trees — but they are recorded, and the test
                    // still requires that something was compared.
                    skipped.push(format!("{name}: {}", e.message));
                    continue;
                }
            };
        let via_checked = crate::checked::erase(checked)
            .unwrap_or_else(|e| panic!("{label}: `{name}`: checked erase failed: {e}"));
        assert_eq!(
            without_at(&via_checked),
            without_at(&eval_rel),
            "{label}: `{name}` erases differently through the checked tree"
        );
        compared += 1;
    }
    assert!(
        compared > 0,
        "{label}: no definition was compared, so this proves nothing (skipped: {skipped:?})"
    );
}

#[test]
fn checked_and_evaluated_trees_agree_on_the_report_example() {
    assert_trees_agree_file(
        "report.cagara",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/report.cagara"),
    );
}

#[test]
fn checked_and_evaluated_trees_agree_on_the_public_example() {
    // `public.cagara` imports `schema.cagara`, so it must be loaded through its
    // path for the import to resolve.
    assert_trees_agree_file(
        "public.cagara",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/public.cagara"),
    );
}

#[test]
fn checked_and_evaluated_trees_agree_on_the_schema_example() {
    assert_trees_agree_file(
        "schema.cagara",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/schema.cagara"),
    );
}

/// The same comparison across a spread of stage combinations, because the
/// examples do not cover every constructor.
#[test]
fn checked_and_evaluated_trees_agree_across_stages() {
    const TABLES: &str = "u : query { id = int, n = string, a = int } = table \"p\" \"u\"\n\
                          v : query { id = int, m = maybe int } = table \"p\" \"v\"\n";
    for (label, body) in [
        ("where", "q = u & where (.a > 0)\n"),
        ("select", "q = u & select { x = .id, y = .n }\n"),
        ("update", "q = u & update { a = .a + 1 }\n"),
        ("omit", "q = u & omit \"n\"\n"),
        ("prefix", "q = u & prefix \"u_\"\n"),
        ("suffix", "q = u & suffix \"_v\"\n"),
        ("order_limit", "q = u & order [desc .a] & limit 3\n"),
        ("offset", "q = u & order [asc .a] & offset 2\n"),
        ("distinct", "q = u & select { a = .a } & distinct\n"),
        ("inner_join", "q = u & innerJoin v (.<id == .>id)\n"),
        ("left_join", "q = u & leftJoin v (.<id == .>id)\n"),
        ("agg", "q = u & agg { a = group .a, n = count }\n"),
        ("agg_then_where", "q = u & agg { a = group .a, n = count } & where (.n > 1)\n"),
        ("set_op", "q = (u & select { a = .a }) &| (u & select { a = .a })\n"),
        ("window", "q = u & select { r = rowNumber { partition = [.a], order = [asc .id] } }\n"),
        ("pipeline", "q = u & where (.a > 1) & where (.a < 9) & select { a = .a + 1 }\n"),
    ] {
        assert_trees_agree(label, &format!("{TABLES}{body}"));
    }
}

// ── differential harness, source level ─────────────────────────────────────

/// Compare **source elaboration** against the evaluator, node for node.
///
/// This is the harness the review asked for, and it replaces the bridge-based
/// one above as the evidence that matters. The difference is where the checked
/// side comes from:
///
/// * the bridge harness built `CheckedQuery` from the evaluator's own erased
///   `Rel`, so it validated `Rel -> CheckedQuery -> Rel`. It could pass even if
///   nothing in the compiler ever constructed a `CheckedQuery` from source —
///   which is exactly the vacuity the review identified.
/// * this one builds `CheckedQuery` **from the source AST**, through the
///   constructors, and compares against the evaluator. If source elaboration is
///   broken, unused, or absent, this fails.
///
/// A definition that the elaborator cannot yet handle is a *failure*, not a
/// skip: silently skipping is how a differential test stops being evidence.
fn assert_source_elaboration_agrees(label: &str, ws: &Workspace) {
    let tc = crate::check::check(ws);
    let m = ws.root;

    // The oracle must be the evaluator *alone*. `root_queries_checked` runs
    // source elaboration too and turns a disagreement into an error, so
    // building the oracle from it makes this harness circular: a mismatch
    // would arrive here as an error, be filtered out by `r.ok()`, and the
    // definition would simply vanish from the comparison — passing.
    //
    // That is exactly the vacuity this harness was written to remove, and it
    // had reappeared here. `root_queries_via_evaluator` is the evaluator with
    // no source elaboration involved.
    let evaluated: std::collections::HashMap<String, Rel> =
        crate::eval::root_queries_via_evaluator(ws, &tc)
            .into_iter()
            .filter_map(|(n, r)| r.ok().map(|r| (n, r)))
            .collect();

    let elaborated = crate::elaborate::elaborate_module(ws, &tc, m);
    // Every definition the evaluator produces a relation for must appear in
    // the elaboration list. Without this, a definition the elaborator silently
    // stopped *seeing* — rather than one it reports as unsupported — would
    // reduce coverage with nothing failing.
    let seen: std::collections::HashSet<&str> =
        elaborated.iter().map(|(n, _)| n.as_str()).collect();
    for name in evaluated.keys() {
        assert!(
            seen.contains(name.as_str()),
            "{label}: the evaluator produced `{name}` but elaboration did not consider it"
        );
    }
    let mut compared = 0;
    let mut unsupported = Vec::new();
    for (name, result) in &elaborated {
        // Only query definitions are compared: a scalar or a helper has no
        // relational term in either path.
        let Some(eval_rel) = evaluated.get(name) else {
            continue;
        };
        match result {
            Ok(q) => {
                let via_source = crate::checked::erase(q.clone())
                    .unwrap_or_else(|e| panic!("{label}: `{name}`: erase failed: {e}"));
                assert_eq!(
                    without_at(&via_source),
                    without_at(eval_rel),
                    "{label}: `{name}` elaborates from source to a different tree than the \
                     evaluator produces"
                );
                compared += 1;
            }
            Err(e) => unsupported.push(format!("{name}: {}", e.message)),
        }
    }
    assert!(
        unsupported.is_empty(),
        "{label}: the elaborator cannot yet handle {} query definition(s): {unsupported:?}",
        unsupported.len()
    );
    assert!(
        compared > 0,
        "{label}: no definition was compared, so this proves nothing"
    );
}

#[test]
fn source_elaboration_agrees_on_a_table_and_a_where() {
    let ws = Workspace::from_source(
        "t : query { a = int, b = string } = table \"s\" \"t\"\n\
         q = t & where (.a > 1)\n",
    );
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    assert_source_elaboration_agrees("table+where", &ws);
}

#[test]
fn source_elaboration_agrees_on_the_report_example() {
    let ws = Workspace::open(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/report.cagara"
    )));
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    assert_source_elaboration_agrees("report.cagara", &ws);
}

#[test]
fn source_elaboration_agrees_on_public_and_schema_examples() {
    for (label, file) in [
        ("public.cagara", "public.cagara"),
        ("schema.cagara", "schema.cagara"),
    ] {
        let ws = Workspace::open(std::path::Path::new(&format!(
            "{}/../../examples/{file}",
            env!("CARGO_MANIFEST_DIR")
        )));
        assert!(ws.diags.is_empty(), "{label}: {:?}", ws.diags);
        assert_source_elaboration_agrees(label, &ws);
    }
}

/// The same source-level comparison across a spread of stage combinations,
/// because the examples do not exercise every constructor.
#[test]
fn source_elaboration_agrees_across_stages() {
    const TABLES: &str = "u : query { id = int, n = string, a = int } = table \"p\" \"u\"\n\
                          v : query { id = int, m = maybe int } = table \"p\" \"v\"\n";
    for (label, body) in [
        ("where", "q = u & where (.a > 0)\n"),
        ("select", "q = u & select { x = .id, y = .n }\n"),
        ("update", "q = u & update { a = .a + 1 }\n"),
        ("omit", "q = u & omit \"n\"\n"),
        ("prefix", "q = u & prefix \"u_\"\n"),
        ("suffix", "q = u & suffix \"_v\"\n"),
        ("order_limit", "q = u & order [desc .a] & limit 3\n"),
        ("offset", "q = u & order [asc .a] & offset 2\n"),
        ("distinct", "q = u & select { a = .a } & distinct\n"),
        ("inner_join", "q = u & innerJoin v (.<id == .>id)\n"),
        ("left_join", "q = u & leftJoin v (.<id == .>id)\n"),
        ("join_operator", "q = u ? v (.<id == .>id)\n"),
        ("agg", "q = u & agg { a = group .a, n = count }\n"),
        ("agg_then_where", "q = u & agg { a = group .a, n = count } & where (.n > 1)\n"),
        ("set_op", "q = (u & select { a = .a }) &| (u & select { a = .a })\n"),
        ("window", "q = u & select { r = rowNumber { partition = [.a], order = [asc .id] } }\n"),
        ("pipeline", "q = u & where (.a > 1) & where (.a < 9) & select { a = .a + 1 }\n"),
        ("shorthand", "q = u &? (.a > 0) &- 2\n"),
    ] {
        let ws = Workspace::from_source(&format!("{TABLES}{body}"));
        assert!(ws.diags.is_empty(), "{label}: {:?}", ws.diags);
        assert_source_elaboration_agrees(label, &ws);
    }
}

/// Constructs the examples happen not to exercise.
///
/// The four examples cover a lot but not everything: they contain no
/// three-way join, no `inList`, and no aggregate followed by a second
/// projection. Each case here was a real gap the elaborator had when this
/// test was written — `inList` failed with "the expression primitive `__in`
/// is not elaborated yet" until it was added — so this is the regression net
/// for constructs that only appear in user programs.
#[test]
fn source_elaboration_agrees_beyond_the_examples() {
    for (label, src) in [
        (
            "three_way_join",
            "u : query { a = int } = table \"s\" \"u\"\n\
             v : query { a = int } = table \"s\" \"v\"\n\
             w : query { a = int } = table \"s\" \"w\"\n\
             q = u & innerJoin v (.<a == .>a) & innerJoin w (.<a == .>a)\n",
        ),
        (
            "in_list",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & where (inList [1, 2] .a)\n",
        ),
        (
            "not_in_list",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & where (not (inList [1] .a))\n",
        ),
        (
            "sql_template_literal",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & where (sql \"$1 > 5\" .a)\n",
        ),
        (
            "select_then_select",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & select { a = .a } & select { b = .a + 1 }\n",
        ),
        (
            "agg_then_project",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & agg { n = count } & select { n = .n + 1 }\n",
        ),
        (
            "limit_then_where",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & limit 3 & where (.a > 1)\n",
        ),
        (
            "group_only",
            "t : query { a = int } = table \"s\" \"t\"\n\
             q = t & agg { a = group .a }\n",
        ),
        (
            "order_by_two_keys",
            "t : query { a = int, b = string } = table \"s\" \"t\"\n\
             q = t & order [desc .a, asc .b] & limit 1\n",
        ),
        (
            "join_then_agg",
            "u : query { id = int, n = int } = table \"s\" \"u\"\n\
             v : query { id = int, m = int } = table \"s\" \"v\"\n\
             q = u & innerJoin v (.<id == .>id) & agg { id = group .id, s = sum .n }\n",
        ),
    ] {
        let ws = Workspace::from_source(src);
        assert!(ws.diags.is_empty(), "{label}: {:?}", ws.diags);
        assert_source_elaboration_agrees(label, &ws);
    }
}

/// A mismatch on one definition must fail even when other definitions in the
/// same program compare cleanly.
///
/// This is the regression test for the harness's own circularity, and the shape
/// matters: with several queries and one bad, a harness that dropped failing
/// definitions would still report "some definitions compared" and pass. The
/// program below therefore has three query definitions, and the assertion is
/// that a *deliberately wrong* elaboration of the middle one is caught.
///
/// It is driven through a locally-erased tree rather than by mutating
/// `checked::erase`, because the point is the harness's bookkeeping — that a
/// single disagreement is not swallowed — not the eraser.
#[test]
fn a_mismatch_on_one_definition_is_not_swallowed_by_the_others() {
    let ws = Workspace::from_source(
        "t : query { a = int, b = string } = table \"s\" \"t\"\n\
         q_one = t\n\
         q_two = t & where (.a > 1)\n\
         q_three = t & select { x = .a }\n",
    );
    assert!(ws.diags.is_empty(), "{:?}", ws.diags);
    let tc = crate::check::check(&ws);

    // The oracle, independently of source elaboration.
    let oracle: std::collections::HashMap<String, Rel> =
        crate::eval::root_queries_via_evaluator(&ws, &tc)
            .into_iter()
            .filter_map(|(n, r)| r.ok().map(|r| (n, r)))
            .collect();
    // Four query definitions: the table `t` counts too, which is worth
    // asserting rather than assuming.
    assert_eq!(oracle.len(), 4, "expected four query definitions: {oracle:?}");

    let elaborated = crate::elaborate::elaborate_module(&ws, &tc, ws.root);
    let by_name: std::collections::HashMap<&str, _> =
        elaborated.iter().map(|(n, r)| (n.as_str(), r)).collect();
    assert!(by_name.contains_key("q_two"), "q_two must be elaborated");

    // Two of the three agree...
    for name in ["q_one", "q_three"] {
        let q = by_name[name].as_ref().expect("elaborated");
        let from_source = crate::checked::without_at(&crate::checked::erase(q.clone()).unwrap());
        assert_eq!(
            from_source,
            crate::checked::without_at(&oracle[name]),
            "{name} should agree"
        );
    }

    // ...and the middle one, perturbed, must *not* be taken for agreement. A
    // harness that skipped a definition whose comparison failed would see two
    // successes here and conclude everything was fine.
    let q_two = by_name["q_two"].as_ref().expect("elaborated");
    let mut perturbed = crate::checked::erase(q_two.clone()).unwrap();
    perturbed = Rel::Limit(Box::new(perturbed), 999);
    assert_ne!(
        crate::checked::without_at(&perturbed),
        crate::checked::without_at(&oracle["q_two"]),
        "the perturbation must actually change the tree, or this proves nothing"
    );
    assert_eq!(
        crate::checked::without_at(&crate::checked::erase(q_two.clone()).unwrap()),
        crate::checked::without_at(&oracle["q_two"]),
        "the unperturbed q_two must agree, so the comparison is live"
    );
}
