//! Dialect rewriting for the whole statement.
//!
//! The IR and `sql` templates are ANSI. sqlglot's `dialects::transform` only
//! rewrites the outermost SELECT's columns / WHERE / GROUP BY / HAVING, so
//! this walks every block (derived tables, CTE bodies, set-operation
//! branches, join inputs) and every expression slot (including ORDER BY and
//! join ON), applying sqlglot's expression rewrites to each. String
//! concatenation is lowered here: `||` means OR in MySQL, so the MySQL and
//! T-SQL families get `CONCAT(a, b, ...)`. Cagara's date and string
//! intrinsics (`CAGARA_*`) are lowered here too, for every dialect including
//! ANSI.

use crate::stage::transform_deep;
use sqlglot_rust::ast::{
    BinaryOperator, Expr, OrderByItem, QuoteStyle, SelectItem, SelectStatement, TableSource,
    UnaryOperator,
};
use sqlglot_rust::{Dialect, Statement};

/// The dialect grouping shared by statement rewriting and intrinsic lowering.
///
/// Both phases need "which engine family is this", and they must agree: the
/// MySQL family gets `CONCAT` *and* the MySQL intrinsic spellings. One map here
/// is the single place a new dialect is classified.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fam {
    Ansi,
    Mysql,
    Sqlite,
    Duck,
    Tsql,
    BigQuery,
    Snowflake,
    Trino,
    Spark,
}

pub(crate) fn fam(d: Dialect) -> Fam {
    match d {
        Dialect::Mysql | Dialect::Doris | Dialect::SingleStore | Dialect::StarRocks => Fam::Mysql,
        Dialect::Sqlite => Fam::Sqlite,
        Dialect::DuckDb => Fam::Duck,
        Dialect::Tsql | Dialect::Fabric => Fam::Tsql,
        Dialect::BigQuery => Fam::BigQuery,
        Dialect::Snowflake => Fam::Snowflake,
        Dialect::Trino | Dialect::Presto | Dialect::Athena => Fam::Trino,
        Dialect::Spark | Dialect::Databricks => Fam::Spark,
        _ => Fam::Ansi,
    }
}

pub fn rewrite(stmt: Statement, to: Dialect) -> Result<Statement, String> {
    let Statement::Select(mut sel) = stmt else {
        return Ok(stmt);
    };
    block(&mut sel, to)?;
    Ok(Statement::Select(sel))
}

fn tsql(d: Dialect) -> bool {
    fam(d) == Fam::Tsql
}

fn mysql(d: Dialect) -> bool {
    fam(d) == Fam::Mysql
}

fn concat_as_function(d: Dialect) -> bool {
    matches!(fam(d), Fam::Mysql | Fam::Tsql)
}

/// Dialects whose string literals treat `\` as an escape character, so a
/// literal `\'` would end the string early.
///
/// This cuts across [`Fam`] (Hive has no family of its own), so it is its own
/// list rather than a family test.
fn backslash_escapes(d: Dialect) -> bool {
    mysql(d)
        || matches!(
            d,
            Dialect::BigQuery
                | Dialect::Snowflake
                | Dialect::Hive
                | Dialect::Spark
                | Dialect::Databricks
                | Dialect::ClickHouse
        )
}

fn block(sel: &mut SelectStatement, to: Dialect) -> Result<(), String> {
    // Inner blocks first: CTE bodies, then the FROM / JOIN inputs.
    for cte in &mut sel.ctes {
        nested(&mut cte.query, to)?;
    }
    if let Some(from) = &mut sel.from {
        source(&mut from.source, to)?;
    }
    for j in &mut sel.joins {
        source(&mut j.table, to)?;
    }

    // `offset` alone: a lone OFFSET is not valid everywhere, so spell the
    // "no limit" the dialect needs.
    //
    //   * MySQL has no OFFSET of its own; its documented idiom is the largest
    //     BIGINT UNSIGNED as the LIMIT.
    //   * SQLite rejects a bare `OFFSET` outright, and *also* rejects the
    //     MySQL trick above (`datatype mismatch`): its spelling is `LIMIT -1`.
    //   * T-SQL rejects `OFFSET n ROWS` with no FETCH — the two are one
    //     syntactic unit — and additionally requires an ORDER BY. Setting the
    //     limit here is what makes sqlglot emit
    //     `OFFSET n ROWS FETCH NEXT m ROWS ONLY`.
    if sel.offset.is_some() && sel.limit.is_none() {
        if mysql(to) {
            sel.limit = Some(Expr::Number("18446744073709551615".into()));
        } else if matches!(to, Dialect::Sqlite) {
            sel.limit = Some(Expr::Number("-1".into()));
        } else if tsql(to) {
            if sel.order_by.is_empty() {
                return Err("T-SQL needs an `order` before `offset`".into());
            }
            // `FETCH NEXT` needs a row count; mimic sqlglot's own choice for
            // "all remaining rows" on this dialect.
            sel.limit = Some(Expr::Number("9223372036854775807".into()));
        }
    }

    // Every expression slot of this block.
    for item in &mut sel.columns {
        if let SelectItem::Expr { expr: e, .. } = item {
            *e = expr(std::mem::replace(e, Expr::Null), to)?;
            // T-SQL has no boolean values, only conditions: a condition as a
            // column becomes 1 / 0, and stays NULL when it is unknown.
            if tsql(to) && is_condition(e) {
                let p = std::mem::replace(e, Expr::Null);
                *e = Expr::Case {
                    operand: None,
                    when_clauses: vec![
                        (p.clone(), Expr::Number("1".into())),
                        (not(p), Expr::Number("0".into())),
                    ],
                    else_clause: None,
                };
            }
        }
    }
    nulls_last(&mut sel.order_by, to);
    for e in [&mut sel.where_clause, &mut sel.having]
        .into_iter()
        .flatten()
    {
        *e = expr(std::mem::replace(e, Expr::Null), to)?;
    }
    for e in &mut sel.group_by {
        *e = expr(std::mem::replace(e, Expr::Null), to)?;
    }
    for o in &mut sel.order_by {
        o.expr = expr(std::mem::replace(&mut o.expr, Expr::Null), to)?;
    }
    for j in &mut sel.joins {
        if let Some(on) = &mut j.on {
            *on = expr(std::mem::replace(on, Expr::Null), to)?;
        }
    }

    // Statement-level rewrites (LIMIT → TOP / FETCH, quoting) for this block.
    // Expressions are already rewritten; sqlglot's pass over them is a no-op.
    if to == Dialect::Ansi {
        return Ok(());
    }
    let Statement::Select(s) = sqlglot_rust::dialects::transform(
        &Statement::Select(std::mem::replace(sel, empty())),
        Dialect::Ansi,
        to,
    ) else {
        // sqlglot's transform is supposed to pass a SELECT back through; a
        // future version that does not must be an error, not an empty block.
        return Err("internal: dialect transform dropped a SELECT block".into());
    };
    *sel = s;
    Ok(())
}

/// A statement nested in a query: a derived table, a CTE body, or a branch of
/// a set operation. A set operation is not a `Select`, so its own ORDER BY /
/// LIMIT / OFFSET are rewritten here; everything else is reached by `block`.
fn nested(s: &mut Statement, to: Dialect) -> Result<(), String> {
    match s {
        Statement::Select(sel) => block(sel, to),
        Statement::SetOperation(set) => {
            nested(&mut set.left, to)?;
            nested(&mut set.right, to)?;
            nulls_last(&mut set.order_by, to);
            for o in &mut set.order_by {
                o.expr = expr(std::mem::replace(&mut o.expr, Expr::Null), to)?;
            }
            for e in [&mut set.limit, &mut set.offset].into_iter().flatten() {
                *e = expr(std::mem::replace(e, Expr::Null), to)?;
            }
            Ok(())
        }
        // Cagara builds only SELECT and set-operation trees.
        _ => Ok(()),
    }
}

fn source(src: &mut TableSource, to: Dialect) -> Result<(), String> {
    match src {
        TableSource::Subquery { query, .. } => nested(query, to)?,
        // sqlglot writes a table's schema as is, so quote it here.
        TableSource::Table(t) => {
            if let Some(s) = t.schema.as_mut().filter(|s| crate::stage::needs_quotes(s)) {
                *s = quote(s, to);
            }
        }
        _ => {}
    }
    Ok(())
}

/// An identifier in the dialect's quotes, the quote character doubled.
fn quote(name: &str, to: Dialect) -> String {
    let (open, close) = match QuoteStyle::for_dialect(to) {
        QuoteStyle::Backtick => ('`', '`'),
        QuoteStyle::Bracket => ('[', ']'),
        _ => ('"', '"'),
    };
    let escaped = name.replace(close, &format!("{close}{close}"));
    format!("{open}{escaped}{close}")
}

/// One expression: intrinsics, concat lowering, then sqlglot's
/// per-expression rewrites (reached by wrapping it in a one-column SELECT).
fn expr(e: Expr, to: Dialect) -> Result<Expr, String> {
    let e = crate::intrinsics::lower(e, to)?;
    if to == Dialect::Ansi {
        return Ok(e);
    }
    let e = if backslash_escapes(to) {
        transform_deep(e, &|e| match e {
            Expr::StringLiteral(s) => Expr::StringLiteral(s.replace('\\', "\\\\")),
            other => other,
        })
    } else {
        e
    };
    let e = if concat_as_function(to) {
        transform_deep(e, &concat)
    } else {
        e
    };
    // Window specs sort NULLs last too.
    let e = transform_deep(e, &|e| match e {
        Expr::Function {
            name,
            args,
            distinct,
            filter,
            over: Some(mut spec),
            order_by,
            within_group,
        } => {
            nulls_last(&mut spec.order_by, to);
            Expr::Function {
                name,
                args,
                distinct,
                filter,
                over: Some(spec),
                order_by,
                within_group,
            }
        }
        Expr::TypedFunction {
            func,
            filter,
            over: Some(mut spec),
        } => {
            nulls_last(&mut spec.order_by, to);
            Expr::TypedFunction {
                func,
                filter,
                over: Some(spec),
            }
        }
        other => other,
    });
    let mut sel = empty();
    sel.columns = vec![SelectItem::Expr {
        expr: e.clone(),
        alias: None,
        alias_quote_style: Default::default(),
    }];
    // The same policy as `block`: sqlglot's transform is supposed to hand the
    // SELECT back. Returning the untransformed expression instead would emit
    // ANSI syntax for a dialect where it means something else (`||` is OR in
    // MySQL), which is worse than failing.
    match sqlglot_rust::dialects::transform(&Statement::Select(sel), Dialect::Ansi, to) {
        Statement::Select(s) => match s.columns.into_iter().next() {
            Some(SelectItem::Expr { expr, .. }) => Ok(expr),
            _ => Err("internal: dialect transform dropped an expression".into()),
        },
        _ => Err("internal: dialect transform dropped a SELECT block".into()),
    }
}

/// Where a dialect sorts NULLs by default: `(last when ascending, last when
/// descending, has NULLS FIRST / LAST)`, or `None` when it is not known
/// (ANSI leaves it to the implementation).
fn null_order(d: Dialect) -> Option<(bool, bool, bool)> {
    use Dialect::*;
    Some(match d {
        DuckDb | Trino | Presto | Athena => (true, true, true),
        Postgres | Oracle | Snowflake | Redshift | Materialize | RisingWave => (true, false, true),
        Sqlite | BigQuery | Spark | Databricks | Hive => (false, true, true),
        _ if mysql(d) || tsql(d) => (false, true, false),
        _ => return None,
    })
}

/// Sort keys marked NULLS LAST, for the target: the clause is dropped where
/// it is the default, and emulated with a leading `CASE WHEN x IS NULL`
/// key where the dialect has no such clause.
fn nulls_last(items: &mut Vec<OrderByItem>, to: Dialect) {
    let Some((asc_last, desc_last, syntax)) = null_order(to) else {
        return;
    };
    let mut out = Vec::with_capacity(items.len());
    for mut it in std::mem::take(items) {
        if it.nulls_first != Some(false) {
            out.push(it);
            continue;
        }
        let default_last = if it.ascending { asc_last } else { desc_last };
        if default_last {
            it.nulls_first = None;
        } else if !syntax {
            let is_null = Expr::IsNull {
                expr: Box::new(it.expr.clone()),
                negated: false,
            };
            out.push(OrderByItem {
                expr: Expr::Case {
                    operand: None,
                    when_clauses: vec![(is_null, Expr::Number("1".into()))],
                    else_clause: Some(Box::new(Expr::Number("0".into()))),
                },
                ascending: true,
                nulls_first: None,
            });
            it.nulls_first = None;
        }
        out.push(it);
    }
    *items = out;
}

/// An expression that is a condition rather than a value in T-SQL.
fn is_condition(e: &Expr) -> bool {
    match e {
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::Neq
                | BinaryOperator::Lt
                | BinaryOperator::Gt
                | BinaryOperator::LtEq
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        } => true,
        Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::InList { .. }
        | Expr::Between { .. }
        | Expr::Exists { .. } => true,
        Expr::Nested(e) => is_condition(e),
        _ => false,
    }
}

fn not(e: Expr) -> Expr {
    Expr::UnaryOp {
        op: UnaryOperator::Not,
        expr: Box::new(Expr::Nested(Box::new(e))),
    }
}

/// `a || b || c` (any nesting, parenthesized or not) → `CONCAT(a, b, c)`.
fn concat(e: Expr) -> Expr {
    match e {
        Expr::BinaryOp {
            op: BinaryOperator::Concat,
            ..
        } => {
            let mut args = Vec::new();
            flatten(e, &mut args);
            Expr::Function {
                name: "CONCAT".into(),
                args,
                distinct: false,
                filter: None,
                over: None,
                order_by: vec![],
                within_group: false,
            }
        }
        other => other,
    }
}

fn flatten(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Concat,
            right,
        } => {
            flatten(*left, out);
            flatten(*right, out);
        }
        Expr::Function { name, args, .. } if name == "CONCAT" => out.extend(args),
        // A parenthesized concatenation, `x <> (y <> z)`, arrives as
        // `Nested(Function CONCAT)`: the walk above is bottom-up.
        Expr::Nested(inner) if matches!(*inner, Expr::Function { ref name, .. } if name == "CONCAT") => {
            flatten(*inner, out)
        }
        other => out.push(other),
    }
}

fn empty() -> SelectStatement {
    SelectStatement {
        comments: vec![],
        ctes: vec![],
        distinct: false,
        top: None,
        columns: vec![],
        from: None,
        joins: vec![],
        where_clause: None,
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        fetch_first: None,
        qualify: None,
        window_definitions: vec![],
        query_options: None,
    }
}
