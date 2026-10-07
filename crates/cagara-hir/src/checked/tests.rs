use super::*;
use crate::core::{Origin, RowType, ScalarType};
use crate::ir::{Lit, Rel};
use crate::schema;
use cagara_syntax::ast::{Side, Span};

fn origin() -> Origin {
    Origin::new(0, Span { start: 1, end: 2 })
}

fn row() -> RowType {
    RowType::new(vec![
        ("id".into(), ScalarType::Int),
        ("active".into(), ScalarType::Bool),
    ])
}

fn table() -> CheckedQuery {
    CheckedQuery::table("public", "users", Some(row()), origin()).unwrap()
}

#[test]
fn table_erasure_preserves_its_row_and_origin() {
    let rel = erase(table()).unwrap();
    assert!(matches!(
        &rel,
        Rel::At(_, inner) if matches!(inner.as_ref(), Rel::Table { columns: Some(_), .. })
    ));
    assert_eq!(schema::schema(&rel).unwrap(), vec!["id", "active"]);
}

#[test]
fn checked_select_erases_to_the_row_it_records() {
    let query = CheckedQuery::select(
        table(),
        vec![(
            "user_id".into(),
            CheckedExpr::column(Side::Single, "id", ScalarType::Int, origin()),
        )],
        origin(),
    )
    .unwrap();
    assert_eq!(query.row().names(), vec!["user_id"]);
    let (rel, columns) = erase_checked(query).unwrap();
    assert_eq!(columns, vec!["user_id"]);
    assert!(matches!(rel, Rel::At(_, inner) if matches!(*inner, Rel::Select(..))));
}

#[test]
fn checked_where_rejects_non_bool_and_missing_columns() {
    let not_bool = CheckedExpr::lit(Lit::Int(1), origin());
    assert!(CheckedQuery::where_(table(), not_bool, origin())
        .unwrap_err()
        .message
        .contains("must be bool"));

    let missing = CheckedExpr::column(Side::Single, "missing", ScalarType::Bool, origin());
    assert!(CheckedQuery::where_(table(), missing, origin())
        .unwrap_err()
        .message
        .contains("no column"));
}
