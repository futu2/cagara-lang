//! Dialect rewriting for the whole statement.
//!
//! The IR and `sql` templates are ANSI. sqlglot's `dialects::transform` only
//! rewrites the outermost SELECT's columns / WHERE / GROUP BY / HAVING, so
//! this walks every block (derived tables, join inputs) and every expression
//! slot (including ORDER BY and join ON), applying sqlglot's expression
//! rewrites to each. String concatenation is lowered here: `||` means OR in
//! MySQL, so the MySQL and T-SQL families get `CONCAT(a, b, ...)`.

use sqlglot_rust::ast::{BinaryOperator, Expr, SelectItem, SelectStatement, TableSource};
use sqlglot_rust::{Dialect, Statement};

pub fn rewrite(stmt: Statement, to: Dialect) -> Result<Statement, String> {
    if to == Dialect::Ansi {
        return Ok(stmt);
    }
    let Statement::Select(mut sel) = stmt else { return Ok(stmt) };
    block(&mut sel, to)?;
    Ok(Statement::Select(sel))
}

fn tsql(d: Dialect) -> bool {
    matches!(d, Dialect::Tsql | Dialect::Fabric)
}

fn mysql(d: Dialect) -> bool {
    matches!(d, Dialect::Mysql | Dialect::Doris | Dialect::SingleStore | Dialect::StarRocks)
}

fn concat_as_function(d: Dialect) -> bool {
    mysql(d) || tsql(d)
}

fn block(sel: &mut SelectStatement, to: Dialect) -> Result<(), String> {
    // Inner blocks first.
    if let Some(from) = &mut sel.from {
        source(&mut from.source, to)?;
    }
    for j in &mut sel.joins {
        source(&mut j.table, to)?;
    }

    // `offset` alone: MySQL needs a LIMIT (its documented idiom is the
    // largest BIGINT UNSIGNED); T-SQL's OFFSET needs an ORDER BY.
    if sel.offset.is_some() && sel.limit.is_none() {
        if mysql(to) {
            sel.limit = Some(Expr::Number("18446744073709551615".into()));
        }
        if tsql(to) && sel.order_by.is_empty() {
            return Err("T-SQL needs an `order` before `offset`".into());
        }
    }

    // Every expression slot of this block.
    let ex = |e: &mut Expr| *e = expr(std::mem::replace(e, Expr::Null), to);
    for item in &mut sel.columns {
        if let SelectItem::Expr { expr: e, .. } = item {
            ex(e);
        }
    }
    sel.where_clause.as_mut().map(ex);
    sel.having.as_mut().map(ex);
    sel.group_by.iter_mut().for_each(ex);
    for o in &mut sel.order_by {
        ex(&mut o.expr);
    }
    for j in &mut sel.joins {
        j.on.as_mut().map(ex);
    }

    // Statement-level rewrites (LIMIT → TOP / FETCH, quoting) for this block.
    // Expressions are already rewritten; sqlglot's pass over them is a no-op.
    let t = sqlglot_rust::dialects::transform(&Statement::Select(std::mem::replace(sel, empty())), Dialect::Ansi, to);
    if let Statement::Select(s) = t {
        *sel = s;
    }
    Ok(())
}

fn source(src: &mut TableSource, to: Dialect) -> Result<(), String> {
    if let TableSource::Subquery { query, .. } = src {
        if let Statement::Select(sel) = query.as_mut() {
            block(sel, to)?;
        }
    }
    Ok(())
}

/// One expression: concat lowering, then sqlglot's per-expression rewrites
/// (reached by wrapping the expression in a one-column SELECT).
fn expr(e: Expr, to: Dialect) -> Expr {
    let e = if concat_as_function(to) { e.transform(&concat) } else { e };
    let mut sel = empty();
    sel.columns = vec![SelectItem::Expr { expr: e.clone(), alias: None, alias_quote_style: Default::default() }];
    match sqlglot_rust::dialects::transform(&Statement::Select(sel), Dialect::Ansi, to) {
        Statement::Select(s) => match s.columns.into_iter().next() {
            Some(SelectItem::Expr { expr, .. }) => expr,
            _ => e,
        },
        _ => e,
    }
}

/// `a || b || c` (any nesting, parenthesized or not) → `CONCAT(a, b, c)`.
fn concat(e: Expr) -> Expr {
    match e {
        Expr::BinaryOp { op: BinaryOperator::Concat, .. } => {
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
        Expr::BinaryOp { left, op: BinaryOperator::Concat, right } => {
            flatten(*left, out);
            flatten(*right, out);
        }
        Expr::Nested(inner) if matches!(*inner, Expr::BinaryOp { op: BinaryOperator::Concat, .. }) => flatten(*inner, out),
        Expr::Function { name, args, .. } if name == "CONCAT" => out.extend(args),
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
