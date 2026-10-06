//! Tests for the explicit relational constructors: each one builds the `Rel`
//! shape today's evaluator builds for the same program, and each rejects what
//! its rule forbids.

use super::*;
use crate::ir::{Bound as IrBound, JoinKind, SetKind};

fn loc(m: usize) -> Loc {
    Loc {
        module: m,
        span: Span { start: 1, end: 9 },
    }
}

fn col(n: &str) -> CoreTerm {
    CoreTerm::col(Side::Single, n.to_string())
}

fn lcol(n: &str) -> CoreTerm {
    CoreTerm::col(Side::Left, n.to_string())
}

fn rcol(n: &str) -> CoreTerm {
    CoreTerm::col(Side::Right, n.to_string())
}

fn table() -> CoreTerm {
    CoreTerm::table("s".into(), "t".into()).with_columns(Some(vec!["a".into(), "b".into()]))
}

/// The `Rel::Table` every stage above erases onto.
fn base() -> Rel {
    Rel::Table {
        schema: "s".into(),
        name: "t".into(),
        columns: Some(vec!["a".into(), "b".into()]),
    }
}

// ── positive: each constructor erases to the evaluator's `Rel` ─────────────

#[test]
fn table_erases_to_the_ir_table_with_its_columns() {
    assert_eq!(erase_core(table()), base());
}

#[test]
fn table_without_columns_erases_to_none() {
    // `attach_schema` fills this in later; before that it is `None`, exactly
    // as `Prim::Table` builds it today.
    assert_eq!(
        erase_core(CoreTerm::table("s".into(), "t".into())),
        Rel::Table {
            schema: "s".into(),
            name: "t".into(),
            columns: None,
        }
    );
}

#[test]
fn with_columns_is_a_no_op_on_a_non_table() {
    let q = CoreTerm::where_(table(), col("a")).unwrap();
    let same = q.clone().with_columns(Some(vec!["x".into()]));
    assert_eq!(q, same);
}

#[test]
fn table_columns_mut_descends_through_at_for_attach_schema() {
    // This is what `eval::attach_schema` does: find the table under any `At`
    // wrappers and fill its column list from the definition's type.
    let mut t = CoreTerm::table("s".into(), "t".into()).at(loc(1));
    if let Some(c) = t.table_columns_mut() {
        assert!(c.is_none());
        *c = Some(vec!["id".into()]);
    }
    assert_eq!(t.table_columns(), Some(&["id".to_string()][..]));
}

#[test]
fn table_columns_mut_is_none_for_a_non_table() {
    let mut q = CoreTerm::where_(table(), col("a")).unwrap();
    assert!(q.table_columns_mut().is_none());
}

#[test]
fn where_erases_to_rel_where() {
    assert_eq!(
        erase_core(CoreTerm::where_(table(), col("a")).unwrap()),
        Rel::Where(Box::new(base()), Expr::Col(Side::Single, "a".into()))
    );
}

#[test]
fn select_erases_to_rel_select_with_labeled_fields() {
    let q = CoreTerm::select(
        table(),
        vec![("x".into(), col("a")), ("y".into(), col("b"))],
    )
    .unwrap();
    assert_eq!(
        erase_core(q),
        Rel::Select(
            Box::new(base()),
            vec![
                ("x".into(), Expr::Col(Side::Single, "a".into())),
                ("y".into(), Expr::Col(Side::Single, "b".into())),
            ],
        )
    );
}

#[test]
fn update_erases_to_rel_update() {
    let q = CoreTerm::update(table(), vec![("a".into(), CoreTerm::lit(Lit::Int(1)))]).unwrap();
    assert_eq!(
        erase_core(q),
        Rel::Update(Box::new(base()), vec![("a".into(), Expr::Lit(Lit::Int(1)))])
    );
}

#[test]
fn omit_prefix_suffix_limit_offset_distinct_erase_directly() {
    assert_eq!(
        erase_core(CoreTerm::omit(table(), "a".into()).unwrap()),
        Rel::Omit(Box::new(base()), "a".into())
    );
    assert_eq!(
        erase_core(CoreTerm::prefix(table(), "u_".into()).unwrap()),
        Rel::Prefix(Box::new(base()), "u_".into())
    );
    assert_eq!(
        erase_core(CoreTerm::suffix(table(), "_v2".into()).unwrap()),
        Rel::Suffix(Box::new(base()), "_v2".into())
    );
    assert_eq!(
        erase_core(CoreTerm::limit(table(), 3).unwrap()),
        Rel::Limit(Box::new(base()), 3)
    );
    assert_eq!(
        erase_core(CoreTerm::offset(table(), 4).unwrap()),
        Rel::Offset(Box::new(base()), 4)
    );
    assert_eq!(
        erase_core(CoreTerm::distinct(table()).unwrap()),
        Rel::Distinct(Box::new(base()))
    );
}

#[test]
fn agg_and_order_erase_with_their_expressions() {
    let agg = CoreTerm::agg(
        table(),
        vec![(
            "n".into(),
            CoreTerm::agg_expr("COUNT(*)".into(), vec![]).unwrap(),
        )],
    )
    .unwrap();
    assert_eq!(
        erase_core(agg),
        Rel::Agg(
            Box::new(base()),
            vec![("n".into(), Expr::Agg("COUNT(*)".into(), vec![]))],
        )
    );

    let ord = CoreTerm::order(table(), vec![(col("a"), false)]).unwrap();
    assert_eq!(
        erase_core(ord),
        Rel::Order(
            Box::new(base()),
            vec![(Expr::Col(Side::Single, "a".into()), false)],
        )
    );
}

#[test]
fn join_erases_to_rel_join_with_a_sided_predicate() {
    let on = CoreTerm::tpl("$1 = $2".into(), vec![lcol("a"), rcol("c")]);
    let q = CoreTerm::join(JoinKind::Inner, table(), table(), on).unwrap();
    assert_eq!(
        erase_core(q),
        Rel::Join {
            kind: JoinKind::Inner,
            left: Box::new(base()),
            right: Box::new(base()),
            on: Expr::Tpl(
                "$1 = $2".into(),
                vec![
                    Expr::Col(Side::Left, "a".into()),
                    Expr::Col(Side::Right, "c".into()),
                ]
            ),
        }
    );
}

#[test]
fn set_erases_to_rel_set() {
    assert_eq!(
        erase_core(CoreTerm::set(SetKind::Union, table(), table())),
        Rel::Set {
            kind: SetKind::Union,
            left: Box::new(base()),
            right: Box::new(base()),
        }
    );
}

#[test]
fn at_erases_to_rel_at_and_only_where_the_caller_put_it() {
    // Constructors never add `At`; only `CoreTerm::at` does, so the erased
    // tree keeps the evaluator's exact wrapper placement.
    let plain = CoreTerm::where_(table(), col("a")).unwrap();
    assert!(matches!(erase_core(plain.clone()), Rel::Where(..)));

    let wrapped = plain.at(loc(2));
    assert_eq!(
        erase_core(wrapped),
        Rel::At(
            loc(2),
            Box::new(Rel::Where(
                Box::new(base()),
                Expr::Col(Side::Single, "a".into())
            ))
        )
    );
}

#[test]
fn at_does_not_double_wrap() {
    let once = table().at(loc(1));
    let twice = once.clone().at(loc(2));
    assert_eq!(once, twice);
}

#[test]
fn expressions_erase_to_ir_expressions() {
    assert_eq!(
        erase_expr_core(col("a")),
        Expr::Col(Side::Single, "a".into())
    );
    assert_eq!(
        erase_expr_core(CoreTerm::lit(Lit::Int(7))),
        Expr::Lit(Lit::Int(7))
    );
    assert_eq!(
        erase_expr_core(CoreTerm::tpl("$1".into(), vec![col("a")])),
        Expr::Tpl("$1".into(), vec![Expr::Col(Side::Single, "a".into())])
    );
    assert_eq!(
        erase_expr_core(CoreTerm::in_(col("a"), vec![CoreTerm::lit(Lit::Int(1))], false).unwrap()),
        Expr::In(
            Box::new(Expr::Col(Side::Single, "a".into())),
            vec![Expr::Lit(Lit::Int(1))],
            false
        )
    );
    assert_eq!(
        erase_expr_core(CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")]).unwrap()),
        Expr::Agg("SUM($1)".into(), vec![Expr::Col(Side::Single, "a".into())])
    );
    assert_eq!(
        erase_expr_core(CoreTerm::group(col("a")).unwrap()),
        Expr::Group(Box::new(Expr::Col(Side::Single, "a".into())))
    );
}

#[test]
fn win_erases_to_ir_win_with_its_spec() {
    let frame = Frame {
        start: IrBound::UnboundedPreceding,
        end: IrBound::CurrentRow,
    };
    let spec = CoreSpec::new(vec![col("a")], vec![(CoreTerm::dir(col("b"), false), false)], Some(frame));
    let w = CoreTerm::win("ROW_NUMBER()".into(), vec![], spec).unwrap();
    assert_eq!(
        erase_expr_core(w),
        Expr::Win(
            "ROW_NUMBER()".into(),
            vec![],
            Box::new(WinSpec {
                partition: vec![Expr::Col(Side::Single, "a".into())],
                order: vec![(Expr::Col(Side::Single, "b".into()), false)],
                frame: Some(frame),
            })
        )
    );
}

#[test]
fn a_dir_key_erases_to_the_key_expression() {
    // `dir` is not a standalone expression: `order` carries the direction in
    // its own flag, so erasing the key drops the `Dir` wrapper.
    let q = CoreTerm::order(table(), vec![(CoreTerm::dir(col("a"), false), false)]).unwrap();
    match erase_core(q) {
        Rel::Order(_, keys) => {
            assert_eq!(keys, vec![(Expr::Col(Side::Single, "a".into()), false)])
        }
        o => panic!("expected an order, got {o:?}"),
    }
}

#[test]
fn rows_accepts_a_well_formed_frame_and_rejects_impossible_ones() {
    let f = CoreTerm::rows(IrBound::Preceding(1), IrBound::Following(2)).unwrap();
    assert_eq!(
        f,
        CoreTerm::Frame(Frame {
            start: IrBound::Preceding(1),
            end: IrBound::Following(2),
        })
    );
    // The three cases `prims::call` rejects for `__rows`.
    assert!(CoreTerm::rows(IrBound::UnboundedFollowing, IrBound::CurrentRow).is_err());
    assert!(CoreTerm::rows(IrBound::CurrentRow, IrBound::UnboundedPreceding).is_err());
    assert!(CoreTerm::rows(IrBound::Following(2), IrBound::Following(1)).is_err());
}

// ── negative: each constructor rejects what its rule forbids ───────────────

#[test]
fn select_update_and_agg_reject_an_empty_field_list() {
    assert_eq!(
        CoreTerm::select(table(), vec![]).unwrap_err().message,
        "`select` needs at least one field"
    );
    assert_eq!(
        CoreTerm::update(table(), vec![]).unwrap_err().message,
        "`update` needs at least one field"
    );
    assert_eq!(
        CoreTerm::agg(table(), vec![]).unwrap_err().message,
        "`agg` needs at least one field"
    );
}

#[test]
fn select_rejects_a_repeated_field_name() {
    // A row cannot address two columns with one name.
    let e = CoreTerm::select(table(), vec![("x".into(), col("a")), ("x".into(), col("b"))])
        .unwrap_err();
    assert_eq!(e.message, "`select` has two fields named `x`");
}

#[test]
fn agg_rejects_a_row_phase_field_that_is_not_grouped() {
    // `.a` is row phase: an `agg` field must be an aggregate or a constant.
    let e = CoreTerm::agg(table(), vec![("a".into(), col("a"))]).unwrap_err();
    assert!(e.message.contains("not grouped"), "{e}");
}

#[test]
fn join_rejects_a_plain_column_in_the_predicate() {
    // A join predicate must say which input a column comes from.
    let on = CoreTerm::tpl("$1".into(), vec![col("a")]);
    let e = CoreTerm::join(JoinKind::Inner, table(), table(), on).unwrap_err();
    assert_eq!(e.message, rules::needs_side("a"));
}

#[test]
fn join_rejects_an_aggregate_predicate() {
    let on = CoreTerm::agg_expr("BOOL_AND($1)".into(), vec![col("a")]).unwrap();
    let e = CoreTerm::join(JoinKind::Inner, table(), table(), on).unwrap_err();
    assert!(e.message.contains("aggregates"), "{e}");
}

#[test]
fn agg_expr_rejects_a_nested_aggregate() {
    let inner = CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")]).unwrap();
    let e = CoreTerm::agg_expr("MAX($1)".into(), vec![inner]).unwrap_err();
    assert!(e.message.contains("cannot nest"), "{e}");
}

#[test]
fn win_rejects_a_nested_window_in_its_arguments() {
    let w = CoreTerm::win("R()".into(), vec![], CoreSpec::new(vec![], vec![], None)).unwrap();
    let e = CoreTerm::win(
        "LAG($1)".into(),
        vec![w],
        CoreSpec::new(vec![], vec![], None),
    )
    .unwrap_err();
    assert!(e.message.contains("cannot nest"), "{e}");
}

#[test]
fn group_rejects_an_aggregate_key() {
    let a = CoreTerm::agg_expr("COUNT(*)".into(), vec![]).unwrap();
    let e = CoreTerm::group(a).unwrap_err();
    assert!(e.message.contains("cannot nest"), "{e}");
}

#[test]
fn order_rejects_an_aggregate_key() {
    let a = CoreTerm::agg_expr("COUNT(*)".into(), vec![]).unwrap();
    let e = CoreTerm::order(table(), vec![(a, true)]).unwrap_err();
    assert!(e.message.contains("column expressions"), "{e}");
}

#[test]
fn limit_and_offset_reject_negative_counts() {
    assert!(CoreTerm::limit(table(), -1)
        .unwrap_err()
        .message
        .contains("non-negative"));
    assert!(CoreTerm::offset(table(), -1)
        .unwrap_err()
        .message
        .contains("non-negative"));
}

#[test]
fn a_relation_has_no_phase() {
    let e = CoreTerm::phase(&table()).unwrap_err();
    assert!(e.contains("not an expression"), "{e}");
}

#[test]
fn phase_matches_the_ir_phase_rule_for_every_leaf_and_template() {
    assert_eq!(col("a").phase().unwrap(), Phase::Row);
    assert_eq!(CoreTerm::lit(Lit::Int(1)).phase().unwrap(), Phase::Const);
    assert_eq!(
        CoreTerm::tpl("$1".into(), vec![col("a")]).phase().unwrap(),
        Phase::Row
    );
    assert_eq!(
        CoreTerm::tpl("$1".into(), vec![CoreTerm::lit(Lit::Int(1))])
            .phase()
            .unwrap(),
        Phase::Const
    );
    assert_eq!(
        CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")])
            .unwrap()
            .phase()
            .unwrap(),
        Phase::Agg
    );
    assert_eq!(CoreTerm::group(col("a")).unwrap().phase().unwrap(), Phase::Agg);
    assert_eq!(
        CoreTerm::win("R()".into(), vec![], CoreSpec::new(vec![], vec![], None))
            .unwrap()
            .phase()
            .unwrap(),
        Phase::Win
    );
    // A scalar template over an aggregate is an aggregate, which is how
    // `inc : agg (expr r int) -> expr r int = sql "$1 + 1"` stays one.
    assert_eq!(
        CoreTerm::tpl(
            "$1 + 1".into(),
            vec![CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")]).unwrap()]
        )
        .phase()
        .unwrap(),
        Phase::Agg
    );
    // And an aggregate mixed with a row column is a clash, not a phase.
    assert!(CoreTerm::tpl(
        "$1 + $2".into(),
        vec![
            CoreTerm::agg_expr("SUM($1)".into(), vec![col("a")]).unwrap(),
            col("b"),
        ]
    )
    .phase()
    .is_err());
}

#[test]
fn columns_are_collected_in_reading_order() {
    let t = CoreTerm::tpl("$1 = $2".into(), vec![lcol("a"), rcol("c")]);
    assert_eq!(
        t.columns(),
        vec![
            (Side::Left, "a".to_string()),
            (Side::Right, "c".to_string())
        ]
    );
    assert_eq!(table().columns(), vec![]);
}

#[test]
fn bare_strips_at_wrappers() {
    let t = table().at(loc(1)).at(loc(2));
    assert!(matches!(t.bare(), CoreTerm::Table { .. }));
}

#[test]
fn of_rel_and_erase_core_round_trip_for_the_shapes_a_rel_records() {
    let rel = Rel::Where(
        Box::new(Rel::At(
            loc(1),
            Box::new(Rel::Table {
                schema: "s".into(),
                name: "t".into(),
                columns: Some(vec!["a".into()]),
            }),
        )),
        Expr::Col(Side::Single, "a".into()),
    );
    assert_eq!(erase_core(of_rel(rel.clone())), rel);
}

#[test]
fn of_checked_matches_checked_erase() {
    // The two erasure paths agree, which is the invariant that keeps the
    // checked tree and the evaluated tree from drifting.
    use crate::checked::{CheckedExpr, CheckedQuery};
    use crate::core::{Origin, RowType, ScalarType};

    let o = Origin::new(1, Span { start: 3, end: 8 });
    let t = CheckedQuery::table(
        "s",
        "t",
        Some(RowType::new(vec![
            ("a".into(), ScalarType::Int),
            ("b".into(), ScalarType::Int),
        ])),
        o,
    )
    .unwrap();
    let pred = CheckedExpr::column(Side::Single, "a", ScalarType::Bool, o);
    let q = CheckedQuery::where_(t, pred, o).unwrap();

    let via_checked = q.clone().erase();
    let via_core = erase_core(of_checked(q));
    assert_eq!(via_checked, via_core);
}

#[test]
fn erase_core_checked_agrees_with_schema_for_a_small_query() {
    let q = CoreTerm::select(
        table(),
        vec![("x".into(), col("a")), ("y".into(), col("b"))],
    )
    .unwrap();
    let (_, cols) = erase_core_checked(q).unwrap();
    assert_eq!(cols, vec!["x".to_string(), "y".to_string()]);
}
